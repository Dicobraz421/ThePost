//! Политика организации в узле (APP-55, SMTP-40, решение 97): `max_recipients`, заданный через Admin API
//! (`PUT /api/v1/orgs/{org}/policy`), действует на следующий сеанс submission без перезапуска — третий получатель
//! транзакции отклоняется (`452 4.5.3`). До политики — умолчание `Policy` (100 получателей). `send_per_hour`
//! действует и на отправку из JMAP (`EmailSubmission/set`, лимиты отправки доставки узла, SMTP-43): письмо сверх
//! лимита — `451 4.7.1`.
//!
//! Узел профиля SMB (`deploy/config/all.yaml`) против PostgreSQL. Без `TEST_DATABASE_URL` тесты пропускаются.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use thepost::serve::{Running, ServeOptions, start};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};
use tp_contracts::TenantId;
use tp_contracts::ids::DomainId;
use tp_contracts::meta::{AccountScope, AccountStore};
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::{SecretRef, SecretStore};
use tp_meta::{MetaConfig, PgMeta};
use tp_testkit::MemSecretStore;

/// Предел ожидания шага (готовность узла).
const WAIT: Duration = Duration::from_secs(30);

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/testdata")
        .join(name)
}

/// Адрес без `sslmode` — открытый канал (профиль dev).
fn dev_url(url: &str) -> String {
    if url.contains("sslmode=") {
        return url.to_owned();
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}sslmode=disable")
}

/// Адрес без пароля и пароль из `user:password@` (без `%XX` — для адреса CI достаточно).
fn split_password(url: &str) -> (String, Option<String>) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_owned(), None);
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    match authority.rsplit_once('@') {
        Some((userinfo, host)) => match userinfo.split_once(':') {
            Some((user, password)) => (
                format!("{scheme}://{user}@{host}{tail}"),
                Some(password.to_owned()),
            ),
            None => (url.to_owned(), None),
        },
        None => (url.to_owned(), None),
    }
}

/// config.yaml без секций верхнего уровня `names` (их задаёт тест).
fn without_sections(text: &str, names: &[&str]) -> String {
    let mut out = String::new();
    let mut skip = false;
    for line in text.lines() {
        let top = !line.starts_with([' ', '\t']) && !line.trim().is_empty();
        if top {
            skip = names
                .iter()
                .any(|n| line.strip_prefix(n).is_some_and(|r| r.starts_with(':')));
        }
        if !skip {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Конфигурация SMB-профиля из поставки с адресами теста: PostgreSQL, каталоги BLOB и индексов, слушатели на
/// `127.0.0.1:0` (submission — неявный TLS), Admin API и локальный сокет `admin init`, тестовый сертификат.
fn config(dir: &Path, db_url: &str, domain: &str, socket: &Path) -> String {
    let all = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/config/all.yaml"),
    )
    .expect("deploy/config/all.yaml");
    let base = without_sections(&all, &["domain", "database", "blob", "secrets", "http"]);
    assert!(base.contains("roles: [all]"), "{base}");
    let path = |p: PathBuf| p.display().to_string().replace('\\', "/");
    format!(
        "{base}\
domain: {domain}
profile: dev
database:
  url: \"{db_url}\"
  password: {{ secret: \"thepost/db#password\" }}
  tls: {{ allow_plaintext: true }}
blob:
  url: file://{blob}
service_http:
  listen: 127.0.0.1:0
  allow_plaintext: true
tls:
  cert_file: \"{cert}\"
meta:
  events:
    node_ttl: 3s
cluster:
  node_id: org-policy-{pid}
runtime:
  drain_deadline: 5s
smtp:
  hostname: localhost
  inbound:
    - {{ listen: \"127.0.0.1:0\" }}
  submission:
    - {{ listen: \"127.0.0.1:0\", implicit_tls: true }}
imap:
  listeners:
    - {{ listen: \"127.0.0.1:0\", implicit_tls: true }}
sieve:
  managesieve:
    listen:
      - {{ listen: \"127.0.0.1:0\" }}
http:
  jmap: {{ listen: \"127.0.0.1:0\", allow_plaintext: true }}
  admin: {{ listen: \"127.0.0.1:0\", allow_plaintext: true }}
search:
  index:
    dir: \"{index}\"
  indexer:
    commit_interval: 100ms
admin:
  init:
    socket: \"{socket}\"
",
        blob = path(dir.join("blob")),
        cert = path(testdata("cert.pem")),
        index = path(dir.join("index")),
        socket = path(socket.to_path_buf()),
        pid = std::process::id(),
    )
}

/// Отдельная база для теста: состав кластера и разделы журнала событий — только узла теста.
async fn create_database(url: &str) -> (String, String) {
    let name = format!("orgpolicy_{}", DomainId::new().as_uuid().simple());
    let admin = sqlx::PgPool::connect(url).await.expect("admin pool");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
        .execute(&admin)
        .await
        .expect("create database");
    admin.close().await;
    let (head, query) = match url.split_once('?') {
        Some((h, q)) => (h, format!("?{q}")),
        None => (url, String::new()),
    };
    let scheme_end = head.find("://").map_or(0, |i| i + 3);
    let base = match head[scheme_end..].find('/') {
        Some(i) => &head[..scheme_end + i],
        None => head,
    };
    (name.clone(), format!("{base}/{name}{query}"))
}

async fn drop_database(url: &str, name: &str) {
    if let Ok(admin) = sqlx::PgPool::connect(url).await {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await;
        admin.close().await;
    }
}

/// Адрес слушателя роли.
fn addr(running: &Running, role: &str) -> std::net::SocketAddr {
    *running
        .listeners
        .get(role)
        .first()
        .unwrap_or_else(|| panic!("роль {role} не открыла слушатель"))
}

/// `thepost admin init` по локальному сокету узла: ответ узла (`status`, `token`, …).
async fn admin_init(socket: &Path, email: &str) -> Value {
    let mut s = tokio::net::UnixStream::connect(socket)
        .await
        .expect("admin init socket");
    s.write_all(format!("{}\n", json!({ "email": email })).as_bytes())
        .await
        .expect("admin init write");
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.expect("admin init read");
    serde_json::from_slice(&out).expect("admin init reply")
}

/// Запрос HTTP/1.1 Admin API с заголовком `Authorization`: код и тело (ответ без `chunked`).
async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    authorization: &str,
    body: Option<&Value>,
) -> (u16, String) {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut s = TcpStream::connect(addr).await.expect("http connect");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: {authorization}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.expect("http write");
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.expect("http read");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, payload) = text.split_once("\r\n\r\n").expect("http head");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (code, payload.to_owned())
}

fn tls_connector() -> TlsConnector {
    let mut roots = RootCertStore::empty();
    let pem = std::fs::read(testdata("cert.pem")).expect("cert");
    roots
        .add(CertificateDer::from_pem_slice(&pem).expect("pem"))
        .expect("root");
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(cfg))
}

/// Строки ответа SMTP до последней (`NNN ` без дефиса); возвращает код.
async fn smtp_reply<S: AsyncRead + Unpin>(s: &mut S) -> u16 {
    let mut buf = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        s.read_exact(&mut byte).await.expect("smtp read");
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n") {
            let line = String::from_utf8_lossy(&buf).into_owned();
            buf.clear();
            if line.len() >= 4 && line.as_bytes()[3] == b' ' {
                return line[..3].parse().expect("code");
            }
        }
    }
}

async fn smtp_cmd<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, cmd: &str) -> u16 {
    s.write_all(format!("{cmd}\r\n").as_bytes())
        .await
        .expect("smtp write");
    s.flush().await.expect("smtp flush");
    smtp_reply(s).await
}

/// Сеанс submission (неявный TLS, `AUTH PLAIN` паролем приложения): коды ответов на `RCPT TO` трём внешним
/// получателям одной транзакции.
async fn rcpt_codes(addr: std::net::SocketAddr, login: &str, password: &str) -> Vec<u16> {
    let tcp = TcpStream::connect(addr).await.expect("submission connect");
    let name = ServerName::try_from("localhost").expect("name");
    let mut s = tls_connector()
        .connect(name, tcp)
        .await
        .expect("submission tls");
    assert_eq!(smtp_reply(&mut s).await, 220);
    assert_eq!(smtp_cmd(&mut s, "EHLO client.test").await, 250);
    let plain = base64::engine::general_purpose::STANDARD.encode(format!("\0{login}\0{password}"));
    assert_eq!(smtp_cmd(&mut s, &format!("AUTH PLAIN {plain}")).await, 235);
    assert_eq!(smtp_cmd(&mut s, &format!("MAIL FROM:<{login}>")).await, 250);
    let mut codes = Vec::new();
    for rcpt in ["one@other.test", "two@other.test", "three@other.test"] {
        codes.push(smtp_cmd(&mut s, &format!("RCPT TO:<{rcpt}>")).await);
    }
    assert_eq!(smtp_cmd(&mut s, "RSET").await, 250);
    assert_eq!(smtp_cmd(&mut s, "QUIT").await, 221);
    codes
}

/// Сокет `admin init` роль admin забирает из общего для процесса места (`local_admin::bind` → запуск узла): узлы
/// тестов одного процесса запускаются по очереди, иначе сокет достаётся чужому узлу.
static ADMIN_SOCKET: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Узел теста: своя база, администратор (`admin init`) с сессией Admin API и паролем приложения.
struct Node {
    url: String,
    db_name: String,
    _dir: tempfile::TempDir,
    shutdown: CancellationToken,
    running: Running,
    pool: tp_meta::pool::DbPool,
    account: String,
    login: String,
    app_password: String,
    bearer: String,
}

impl Node {
    /// Запуск узла; `None` — без `TEST_DATABASE_URL`.
    async fn start() -> Option<Self> {
        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return None;
        };
        let url = dev_url(&url);
        let (db_name, url_db) = create_database(&url).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let (db_url, password) = split_password(&url_db);
        let secrets = Arc::new(MemSecretStore::default());
        let db: SecretRef = "thepost/db#password".parse().expect("ref");
        secrets.set(&db, password.unwrap_or_else(|| "unused".to_owned()));
        // Ключ TLS — только секрет `tls.key` (PLT-31; ссылка по умолчанию `thepost/tls#key`).
        let key: SecretRef = "thepost/tls#key".parse().expect("ref");
        secrets.set(
            &key,
            std::fs::read_to_string(testdata("key.pem")).expect("key.pem"),
        );

        let domain = "example.ru";
        let socket = dir.path().join("admin.sock");
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, config(dir.path(), &db_url, domain, &socket)).expect("config");
        // Локальный сокет `admin init` открывается до запуска узла (как `startup::prepare_serve`).
        let cfg = thepost::sections::registry()
            .expect("registry")
            .load(&path)
            .expect("load config");
        let bound = ADMIN_SOCKET.lock().await;
        thepost::local_admin::bind(&cfg).expect("admin init socket");
        let shutdown = CancellationToken::new();
        let running = start(ServeOptions {
            config: path,
            secrets: Some(secrets.clone() as Arc<dyn SecretStore>),
            telemetry: false,
            shutdown: shutdown.clone(),
            modules: thepost::modules::BUILTIN,
        })
        .await
        .expect("start");
        let until = tokio::time::Instant::now() + WAIT;
        while !running.health.is_ready() {
            assert!(
                tokio::time::Instant::now() < until,
                "не дождались: узел готов"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Администратор — он же отправитель: учётная запись и принятый домен заведены `admin init`.
        let login = format!("admin@{domain}");
        let granted = admin_init(&socket, &login).await;
        drop(bound);
        assert_eq!(granted["status"], "granted", "{granted}");
        let token = granted["token"].as_str().expect("token").to_owned();
        let (code, body) = http(
            addr(&running, "admin"),
            "POST",
            "/api/v1/session",
            &format!("Bearer {token}"),
            None,
        )
        .await;
        assert_eq!(code, 201, "{body}");
        let session: Value = serde_json::from_str(&body).expect("session");
        let bearer = format!("Bearer {}", session["session"].as_str().expect("session"));

        // Пароль приложения администратора — для AUTH в submission и входа в JMAP.
        let pool = tp_meta::pool::DbPool::new(
            sqlx::PgPool::connect(&url_db).await.expect("pool"),
            Arc::new(tp_meta::pool::NoAcquireObserver),
        );
        let meta = PgMeta::new(pool.clone(), MetaConfig::default());
        let tenant = TenantId::DEFAULT;
        let account = meta
            .by_login(tenant, &login)
            .await
            .expect("by_login")
            .expect("учётная запись admin init");
        let apps =
            tp_auth::AppPasswords::new(Arc::new(meta.clone()), tp_auth::PasswordConfig::default())
                .expect("app passwords");
        let (_, secret) = apps
            .issue(
                AccountScope {
                    tenant,
                    account: account.id,
                },
                "org-policy",
            )
            .await
            .expect("issue");
        Some(Self {
            url,
            db_name,
            _dir: dir,
            shutdown,
            running,
            pool,
            account: account.id.to_string(),
            login,
            app_password: secret.expose().to_owned(),
            bearer,
        })
    }

    /// Admin API: политика организации целиком (`PUT /api/v1/orgs/{org}/policy`).
    async fn org_policy(&self, policy: Value) {
        let (code, body) = http(
            addr(&self.running, "admin"),
            "PUT",
            &format!("/api/v1/orgs/{}/policy", TenantId::DEFAULT),
            &self.bearer,
            Some(&policy),
        )
        .await;
        assert_eq!(code, 200, "{body}");
    }

    /// Вызов одного метода JMAP от администратора; ответ метода (аргументы).
    async fn jmap(&self, method: &str, args: Value) -> Value {
        let req = json!({
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:mail",
                "urn:ietf:params:jmap:submission",
            ],
            "methodCalls": [[method, args, "c0"]],
        });
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.login, self.app_password));
        let (code, body) = http(
            addr(&self.running, "jmap"),
            "POST",
            "/jmap/api",
            &format!("Basic {basic}"),
            Some(&req),
        )
        .await;
        assert_eq!(code, 200, "{body}");
        let resp: Value = serde_json::from_str(&body).expect("json");
        let call = &resp["methodResponses"][0];
        assert_eq!(call[0], method, "{resp}");
        call[1].clone()
    }

    /// Остановка узла и удаление базы.
    async fn finish(self) {
        self.pool.close().await;
        self.shutdown.cancel();
        let report = self.running.wait().await.expect("wait");
        assert!(report.failed.is_empty(), "{report:?}");
        drop_database(&self.url, &self.db_name).await;
    }
}

/// `max_recipients: 2` организации через Admin API — на следующий сеанс submission без перезапуска.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn org_max_recipients_without_restart() {
    let Some(node) = Node::start().await else {
        return;
    };
    let submission = addr(&node.running, "submission");

    // Без политики организации — умолчание `Policy`: все три получателя приняты.
    assert_eq!(
        rcpt_codes(submission, &node.login, &node.app_password).await,
        [250, 250, 250]
    );
    node.org_policy(json!({ "max_recipients": 2 })).await;
    // Следующий сеанс: третий получатель отклонён (`452 4.5.3`).
    assert_eq!(
        rcpt_codes(submission, &node.login, &node.app_password).await,
        [250, 250, 452]
    );
    node.finish().await;
}

/// `send_per_hour: 1` организации — и для JMAP `EmailSubmission/set` (лимиты отправки доставки узла, SMTP-43):
/// первое письмо в очереди, второе за час — `451 4.7.1`, не отправлено.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn org_send_per_hour_jmap() {
    let Some(node) = Node::start().await else {
        return;
    };
    node.org_policy(json!({ "send_per_hour": 1 })).await;
    let account = node.account.clone();
    let identities = node
        .jmap("Identity/get", json!({ "accountId": account }))
        .await;
    let identity = identities["list"][0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("нет идентичности: {identities}"))
        .to_owned();
    let mailboxes = node
        .jmap("Mailbox/get", json!({ "accountId": account }))
        .await;
    let drafts = mailboxes["list"]
        .as_array()
        .and_then(|l| l.iter().find(|m| m["role"] == "drafts"))
        .and_then(|m| m["id"].as_str())
        .unwrap_or_else(|| panic!("нет черновиков: {mailboxes}"))
        .to_owned();
    let rcpt = "rcpt@other.test";
    let mut statuses = Vec::new();
    for n in 0..2 {
        let created = node
            .jmap(
                "Email/set",
                json!({
                    "accountId": account,
                    "create": { "m": {
                        "mailboxIds": { drafts.as_str(): true },
                        "from": [{ "email": node.login }],
                        "to": [{ "email": rcpt }],
                        "subject": format!("send limit {n}"),
                        "textBody": [{ "partId": "t", "type": "text/plain" }],
                        "bodyValues": { "t": { "value": "Hello.\n" } },
                    } },
                }),
            )
            .await;
        let email = created["created"]["m"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("черновик не создан: {created}"))
            .to_owned();
        let submitted = node
            .jmap(
                "EmailSubmission/set",
                json!({
                    "accountId": account,
                    "create": { "s": { "identityId": identity, "emailId": email } },
                }),
            )
            .await;
        statuses.push(submitted["created"]["s"]["deliveryStatus"][rcpt].clone());
    }
    assert_eq!(statuses[0]["delivered"], "queued", "{statuses:?}");
    assert_eq!(statuses[1]["delivered"], "no", "{statuses:?}");
    assert!(
        statuses[1]["smtpReply"]
            .as_str()
            .is_some_and(|r| r.starts_with("451 4.7.1")),
        "{statuses:?}"
    );
    node.finish().await;
}
