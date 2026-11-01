//! Сквозной тест MVP (APP-05, SRS прил. Д.7): узел профиля SMB (`deploy/config/all.yaml`, все роли в одном процессе)
//! против PostgreSQL — приём SMTP → IMAP FETCH и JMAP Email/get → поиск → удаление в корзину → восстановление.
//!
//! Без `TEST_DATABASE_URL` тест пропускается.

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
use tp_contracts::ext::DecisionHooks;
use tp_contracts::ids::DomainId;
use tp_contracts::mailstore::MailStore;
use tp_contracts::meta::{AccountScope, AccountStore, AccountUpsert};
use tp_contracts::retention::{TrashObject, TrashStore};
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::{SecretRef, SecretStore};
use tp_meta::{MetaConfig, PgMeta};
use tp_testkit::MemSecretStore;

/// Предел ожидания каждого шага (индексация, готовность узла).
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

/// Конфигурация SMB-профиля из поставки с адресами теста: PostgreSQL, каталог BLOB и индексов, слушатели на
/// `127.0.0.1:0`, тестовый сертификат.
fn config(dir: &Path, db_url: &str, domain: &str) -> String {
    let all = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/config/all.yaml"),
    )
    .expect("deploy/config/all.yaml");
    let base = without_sections(&all, &["domain", "database", "blob", "secrets"]);
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
  key_file: \"{key}\"
meta:
  events:
    node_ttl: 3s
cluster:
  node_id: e2e-{pid}
  events_poll_interval: 100ms
runtime:
  drain_deadline: 5s
smtp:
  hostname: localhost
  inbound:
    - {{ listen: \"127.0.0.1:0\" }}
  delivery:
    local_domains: [{domain}]
imap:
  listeners:
    - {{ listen: \"127.0.0.1:0\", implicit_tls: true }}
http:
  jmap: {{ listen: \"127.0.0.1:0\", allow_plaintext: true }}
search:
  index:
    dir: \"{index}\"
  indexer:
    commit_interval: 100ms
",
        blob = path(dir.join("blob")),
        cert = path(testdata("cert.pem")),
        key = path(testdata("key.pem")),
        index = path(dir.join("index")),
        pid = std::process::id(),
    )
}

/// Срок ожидания шага: `WAIT` от текущего момента.
fn deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + WAIT
}

/// Пауза перед повтором; срок истёк — тест не пройден.
async fn retry(until: tokio::time::Instant, what: &str) {
    assert!(tokio::time::Instant::now() < until, "не дождались: {what}");
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// Отдельная база для теста: состав кластера и разделы журнала событий — только узла теста (на общей базе
/// живые узлы других тестов получили бы часть разделов). Возвращает имя базы и адрес.
async fn create_database(url: &str) -> (String, String) {
    let name = format!("e2e_{}", DomainId::new().as_uuid().simple());
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

// ---------- SMTP ----------

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

async fn smtp_cmd(s: &mut TcpStream, cmd: &str) -> u16 {
    s.write_all(format!("{cmd}\r\n").as_bytes())
        .await
        .expect("smtp write");
    smtp_reply(s).await
}

async fn smtp_send(addr: std::net::SocketAddr, from: &str, to: &str, message: &str) {
    let mut s = TcpStream::connect(addr).await.expect("smtp connect");
    assert_eq!(smtp_reply(&mut s).await, 220);
    assert_eq!(smtp_cmd(&mut s, "EHLO client.test").await, 250);
    assert_eq!(smtp_cmd(&mut s, &format!("MAIL FROM:<{from}>")).await, 250);
    assert_eq!(smtp_cmd(&mut s, &format!("RCPT TO:<{to}>")).await, 250);
    assert_eq!(smtp_cmd(&mut s, "DATA").await, 354);
    s.write_all(message.replace('\n', "\r\n").as_bytes())
        .await
        .expect("data");
    assert_eq!(smtp_cmd(&mut s, "\r\n.").await, 250, "письмо не принято");
    assert_eq!(smtp_cmd(&mut s, "QUIT").await, 221);
}

// ---------- IMAP ----------

struct Imap<S> {
    s: S,
    tag: u32,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Imap<S> {
    /// Приветствие сервера.
    async fn greeting(&mut self) -> String {
        let mut buf = Vec::new();
        while !buf.ends_with(b"\r\n") {
            let mut byte = [0u8; 1];
            self.s.read_exact(&mut byte).await.expect("imap read");
            buf.push(byte[0]);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Команда и все ответы до тегированного; `OK` обязателен.
    async fn cmd(&mut self, cmd: &str) -> String {
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        self.s
            .write_all(format!("{tag} {cmd}\r\n").as_bytes())
            .await
            .expect("imap write");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = self.s.read(&mut chunk).await.expect("imap read");
            assert!(
                n > 0,
                "imap: соединение закрыто; {}",
                String::from_utf8_lossy(&buf)
            );
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf);
            // Только полностью принятые строки: последняя может прийти по частям.
            let complete = &text[..text.rfind("\r\n").unwrap_or(0)];
            if let Some(line) = complete
                .split("\r\n")
                .find(|l| l.starts_with(&format!("{tag} ")))
            {
                assert!(line.starts_with(&format!("{tag} OK")), "imap {cmd}: {text}");
                return text.into_owned();
            }
        }
    }
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

async fn imap_login(
    addr: std::net::SocketAddr,
    login: &str,
    password: &str,
) -> Imap<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await.expect("imap connect");
    let name = ServerName::try_from("localhost").expect("name");
    let s = tls_connector().connect(name, tcp).await.expect("imap tls");
    let mut imap = Imap { s, tag: 0 };
    assert!(imap.greeting().await.starts_with("* OK"));
    imap.cmd(&format!("LOGIN \"{login}\" \"{password}\"")).await;
    imap
}

/// Число писем из ответа `SELECT` (`* N EXISTS`).
fn exists(select: &str) -> u32 {
    select
        .split("\r\n")
        .find_map(|l| {
            l.strip_prefix("* ")
                .and_then(|r| r.strip_suffix(" EXISTS"))
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or_else(|| panic!("нет EXISTS: {select}"))
}

// ---------- JMAP ----------

/// Запрос HTTP/1.1 с Basic: код и тело (с разбором `chunked`).
async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    auth: &str,
    body: Option<&Value>,
) -> (u16, Vec<u8>) {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut s = TcpStream::connect(addr).await.expect("http connect");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {auth}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.expect("http write");
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.expect("http read");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http head");
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut payload = raw[split + 4..].to_vec();
    if head.contains("transfer-encoding: chunked") {
        let mut out = Vec::new();
        let mut rest = payload.as_slice();
        while let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") {
            let size = usize::from_str_radix(
                String::from_utf8_lossy(&rest[..eol])
                    .split(';')
                    .next()
                    .unwrap_or("0")
                    .trim(),
                16,
            )
            .unwrap_or(0);
            if size == 0 {
                break;
            }
            let start = eol + 2;
            out.extend_from_slice(&rest[start..start + size]);
            rest = &rest[start + size + 2..];
        }
        payload = out;
    }
    (code, payload)
}

struct Jmap {
    addr: std::net::SocketAddr,
    auth: String,
    account: String,
}

impl Jmap {
    /// Вызов одного метода; ответ метода (аргументы).
    async fn call(&self, method: &str, args: Value) -> Value {
        let req = json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [[method, args, "c0"]],
        });
        let (code, body) = http(self.addr, "POST", "/jmap/api", &self.auth, Some(&req)).await;
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let resp: Value = serde_json::from_slice(&body).expect("json");
        let call = &resp["methodResponses"][0];
        assert_eq!(call[0], method, "{resp}");
        call[1].clone()
    }

    /// Письма по фильтру Email/query.
    async fn query(&self, filter: Value) -> Vec<String> {
        let r = self
            .call(
                "Email/query",
                json!({ "accountId": self.account, "filter": filter }),
            )
            .await;
        r["ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Папка с ролью `role` (Mailbox/get).
    async fn mailbox(&self, role: &str) -> String {
        let r = self
            .call("Mailbox/get", json!({ "accountId": self.account }))
            .await;
        r["list"]
            .as_array()
            .and_then(|l| l.iter().find(|m| m["role"] == role))
            .and_then(|m| m["id"].as_str())
            .unwrap_or_else(|| panic!("нет папки {role}: {r}"))
            .to_owned()
    }
}

// ---------- тест ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mvp_mail_flow() {
    let Some(url) = tp_testkit::pg_url() else {
        eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
        return;
    };
    let url = dev_url(&url);
    let (db_name, url_db) = create_database(&url).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (db_url, password) = split_password(&url_db);
    let secrets = Arc::new(MemSecretStore::default());
    let db: SecretRef = "thepost/db#password".parse().expect("ref");
    secrets.set(&db, password.unwrap_or_else(|| "unused".to_owned()));

    // Домен профиля SMB из поставки (`deploy/config/all.yaml`).
    let domain = "example.ru".to_owned();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, config(dir.path(), &db_url, &domain)).expect("config");
    let shutdown = CancellationToken::new();
    let running = start(ServeOptions {
        config: path,
        secrets: Some(secrets.clone() as Arc<dyn SecretStore>),
        telemetry: false,
        shutdown: shutdown.clone(),
    })
    .await
    .expect("start");
    let until = deadline();
    while !running.health.is_ready() {
        retry(until, "узел готов").await;
    }

    // Учётная запись и пароль приложения — в той же БД (миграции выполнил узел).
    let pool = sqlx::PgPool::connect(&url_db).await.expect("pool");
    let meta = PgMeta::new(pool.clone(), MetaConfig::default());
    let tenant = TenantId::DEFAULT;
    let domain_id = DomainId::new();
    let mut tx = tp_meta::schema::begin(&pool, tenant).await.expect("begin");
    sqlx::query("INSERT INTO domains (tenant_id, domain_id, name) VALUES ($1, $2, $3)")
        .bind(tenant.as_uuid())
        .bind(domain_id.as_uuid())
        .bind(&domain)
        .execute(&mut *tx)
        .await
        .expect("domain");
    tx.commit().await.expect("commit");
    let login = format!("alice@{domain}");
    let account = meta
        .upsert(
            tenant,
            AccountUpsert {
                domain: domain_id,
                login: login.clone(),
                display_name: "Alice".into(),
                external_id: None,
                enabled: true,
            },
        )
        .await
        .expect("account");
    let scope = AccountScope {
        tenant,
        account: account.id,
    };
    let apps =
        tp_auth::AppPasswords::new(Arc::new(meta.clone()), tp_auth::PasswordConfig::default())
            .expect("app passwords");
    let (_, secret) = apps.issue(scope, "e2e").await.expect("issue");
    let app_password = secret.expose().to_owned();

    // 1. Приём SMTP.
    let word = format!("zebra{}", std::process::id());
    let subject = "MVP end-to-end check";
    let message = format!(
        "From: Bob <bob@sender.test>\nTo: Alice <{login}>\nSubject: {subject}\n\
         Message-ID: <e2e-{word}@sender.test>\nMIME-Version: 1.0\n\
         Content-Type: text/plain; charset=us-ascii\n\n\
         Search must find this message by the word {word}.\n"
    );
    smtp_send(
        addr(&running, "smtp-in"),
        "bob@sender.test",
        &login,
        &message,
    )
    .await;

    // 2. IMAP FETCH.
    let mut imap = imap_login(addr(&running, "imap"), &login, &app_password).await;
    let until = deadline();
    while exists(&imap.cmd("SELECT INBOX").await) != 1 {
        retry(until, "письмо во «Входящих» (IMAP)").await;
    }
    let fetch = imap.cmd("FETCH 1 (BODY.PEEK[])").await;
    assert!(fetch.contains(&word), "{fetch}");
    assert!(fetch.contains("Subject:"), "{fetch}");

    // 3. JMAP Email/get.
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{login}:{app_password}"));
    let jmap = Jmap {
        addr: addr(&running, "jmap"),
        auth,
        account: account.id.to_string(),
    };
    let inbox = jmap.mailbox("inbox").await;
    let ids = jmap.query(json!({ "inMailbox": inbox })).await;
    assert_eq!(ids.len(), 1, "{ids:?}");
    let got = jmap
        .call(
            "Email/get",
            json!({ "accountId": jmap.account, "ids": ids, "properties": ["subject", "from"] }),
        )
        .await;
    assert_eq!(got["list"][0]["subject"], subject, "{got}");
    assert_eq!(
        got["list"][0]["from"][0]["email"], "bob@sender.test",
        "{got}"
    );

    // 4. Поиск: индексатор обрабатывает журнал событий, запрос — по полнотекстовому индексу.
    let until = deadline();
    let found = loop {
        let found = jmap.query(json!({ "text": word })).await;
        if !found.is_empty() {
            break found;
        }
        retry(until, "письмо найдено поиском (JMAP)").await;
    };
    assert_eq!(found, ids);

    // 5. Удаление в корзину: EXPUNGE переносит письмо в корзину с исходной папкой (SRS 4.7).
    imap.cmd("STORE 1 +FLAGS.SILENT (\\Deleted)").await;
    imap.cmd("EXPUNGE").await;
    assert_eq!(exists(&imap.cmd("SELECT INBOX").await), 0);
    let trash = TrashStore::list(&meta, scope, 0, 10).await.expect("trash");
    assert_eq!(trash.len(), 1, "{trash:?}");
    let id = match &trash[0].object {
        TrashObject::Message(id) => *id,
        other => panic!("не письмо: {other:?}"),
    };
    assert_eq!(id.to_string(), ids[0]);

    // 6. Восстановление (как Admin API: MailStore::restore) — письмо снова во «Входящих» с тем же id.
    let blob = tp_blob::LocalStore::open(tp_blob::LocalConfig {
        root: dir.path().join("blob"),
        ..tp_blob::LocalConfig::default()
    })
    .await
    .expect("blob");
    let store = tp_mailstore::Store::new(tp_mailstore::Deps {
        blobs: Arc::new(blob),
        index: Arc::new(meta.clone()),
        messages: Arc::new(meta.clone()),
        changes: Arc::new(meta.clone()),
        trash: Arc::new(meta.clone()),
        quota: Arc::new(meta.clone()),
        bus: Arc::new(tp_cluster::LocalBus::new(
            &tp_cluster::LocalBusConfig::default(),
        )),
        hooks: DecisionHooks::default(),
    });
    let restored = store.restore(scope, &[id]).await.expect("restore");
    assert_eq!(restored.len(), 1);
    assert_eq!(exists(&imap.cmd("SELECT INBOX").await), 1);
    assert_eq!(jmap.query(json!({ "inMailbox": inbox })).await, ids);
    imap.cmd("LOGOUT").await;

    shutdown.cancel();
    let report = running.wait().await.expect("wait");
    assert!(report.failed.is_empty(), "{report:?}");
    pool.close().await;
    drop_database(&url, &db_name).await;
}
