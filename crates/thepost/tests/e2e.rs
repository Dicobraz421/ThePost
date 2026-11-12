//! Сквозной тест MVP (APP-05, SRS прил. Д.7): узел профиля SMB (`deploy/config/all.yaml`, все роли в одном процессе)
//! против PostgreSQL — приём SMTP (объекты письма хранятся сжатыми, приём и доставка — в журнале доставки) → IMAP
//! FETCH и JMAP Email/get → поиск → удаление в корзину → восстановление → «очистить корзину» в IMAP и JMAP по
//! настройке `trash.purge` → отправка из JMAP локальному адресату (CMP-07) → истечение срока корзины (роль
//! maintenance). Вход — через защиту от перебора. По пути — ManageSieve в роли imap (`STARTTLS`, вход `PLAIN`,
//! `LISTSCRIPTS`) и событие календаря из JMAP (`CalendarEvent/set`), видимое в CalDAV (APP-27).
//!
//! Настройки транспорта без перезапуска (APP-09): доверенный источник, созданный через Admin API (вход — `admin init`
//! по локальному сокету и сессия), действует на следующую сессию SMTP; правила потока почты тестового модуля
//! (`Registry::mail_flow`) вызываются один раз на письмо.
//!
//! Планирование встреч (APP-28): приглашение из CalDAV и JMAP — участнику организации в календарь, внешнему —
//! письмом iMIP в очередь исходящей почты.
//!
//! Метрики Д.7 (APP-34): `/metrics` узла с шиной LISTEN/NOTIFY — пул СУБД, слушатель, фиксации с NOTIFY, «сироты».
//!
//! Без `TEST_DATABASE_URL` тесты пропускаются.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use thepost::modules::Builtin;
use thepost::serve::{Running, ServeOptions, start};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};
use tp_contracts::TenantId;
use tp_contracts::delivery::{DeliveryEventKind, DeliveryLog, DeliveryQuery};
use tp_contracts::ext::{DecisionHooks, Migration, Module, ModuleCtx, Registry};
use tp_contracts::ids::DomainId;
use tp_contracts::mailstore::MailStore;
use tp_contracts::meta::{AccountScope, AccountStore, AccountUpsert, MessageStore, Policy};
use tp_contracts::retention::{TrashObject, TrashPurge, TrashPurgeRule, TrashStore};
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::{SecretRef, SecretStore};
use tp_contracts::transport::{
    AcceptedDomain, DomainKind, FlowInput, FlowOutcome, MailFlow, TransportStore,
};
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
  key_file: \"{key}\"
meta:
  events:
    node_ttl: 3s
cluster:
  node_id: e2e-{pid}
runtime:
  drain_deadline: 5s
smtp:
  hostname: localhost
  inbound:
    - {{ listen: \"127.0.0.1:0\" }}
imap:
  listeners:
    - {{ listen: \"127.0.0.1:0\", implicit_tls: true }}
sieve:
  managesieve:
    listen:
      - {{ listen: \"127.0.0.1:0\" }}
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

/// Принятый домен организации в БД (как `thepost admin domain add`): по нему SMTP доставляет
/// локально (решение 96), настройки `smtp.delivery` доменов не задают.
async fn accept_domain(meta: &PgMeta, tenant: TenantId, name: &str) -> DomainId {
    let id = DomainId::new();
    let version = TransportStore::cluster(meta)
        .await
        .expect("transport")
        .version;
    meta.put_domain(
        version,
        &AcceptedDomain {
            id,
            tenant,
            name: name.to_owned(),
            kind: DomainKind::Authoritative,
        },
    )
    .await
    .expect("accepted domain");
    id
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

// ---------- ManageSieve ----------

/// Ответ ManageSieve до строки итога (`OK`, `NO`, `BYE`) включительно.
async fn sieve_reply<S: AsyncRead + Unpin>(s: &mut S) -> String {
    let mut buf = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        s.read_exact(&mut byte).await.expect("managesieve read");
        buf.push(byte[0]);
        if !buf.ends_with(b"\r\n") {
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let last = text.trim_end().rsplit("\r\n").next().unwrap_or_default();
        if ["OK", "NO", "BYE"].iter().any(|k| last.starts_with(k)) {
            return text.into_owned();
        }
    }
}

/// Команда ManageSieve и её ответ.
async fn sieve_cmd<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, cmd: &str) -> String {
    s.write_all(format!("{cmd}\r\n").as_bytes())
        .await
        .expect("managesieve write");
    sieve_reply(s).await
}

/// Итог ответа ManageSieve — `OK`.
fn sieve_ok(reply: &str) -> bool {
    reply
        .trim_end()
        .rsplit("\r\n")
        .next()
        .is_some_and(|l| l.starts_with("OK"))
}

// ---------- JMAP ----------

/// Запрос HTTP/1.1 с заголовком `Authorization` (`Basic …`, `Bearer …`): код и тело (с разбором `chunked`).
async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    authorization: &str,
    body: Option<&Value>,
) -> (u16, Vec<u8>) {
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

/// Запрос DAV без тела: ответ целиком (строка состояния, заголовки, тело как есть).
async fn dav(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    authorization: &str,
    depth: &str,
) -> String {
    let mut s = TcpStream::connect(addr).await.expect("dav connect");
    let auth = if authorization.is_empty() {
        String::new()
    } else {
        format!("Authorization: {authorization}\r\n")
    };
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Depth: {depth}\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.expect("dav write");
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.expect("dav read");
    String::from_utf8_lossy(&raw).into_owned()
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
            "using": [
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:mail",
                "urn:ietf:params:jmap:submission",
                "urn:ietf:params:jmap:calendars",
            ],
            "methodCalls": [[method, args, "c0"]],
        });
        let basic = format!("Basic {}", self.auth);
        let (code, body) = http(self.addr, "POST", "/jmap/api", &basic, Some(&req)).await;
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
    // Роль admin узла забирает ожидающий сокет `admin init`, чей бы он ни был: запуск — не одновременно с тестами
    // Admin API.
    let bound = ADMIN_SOCKET.lock().await;
    let running = start(ServeOptions {
        config: path,
        secrets: Some(secrets.clone() as Arc<dyn SecretStore>),
        telemetry: false,
        shutdown: shutdown.clone(),
        modules: thepost::modules::BUILTIN,
    })
    .await
    .expect("start");
    drop(bound);
    let until = deadline();
    while !running.health.is_ready() {
        retry(until, "узел готов").await;
    }

    // Учётная запись и пароль приложения — в той же БД (миграции выполнил узел).
    let pool = db_pool(&url_db).await;
    let meta = PgMeta::new(pool.clone(), MetaConfig::default());
    let tenant = TenantId::DEFAULT;
    let domain_id = accept_domain(&meta, tenant, &domain).await;
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
                aliases: Vec::new(),
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
         Search must find this message by the word {word}.\n{filler}",
        // Тело больше `blob.compression.min_bytes` и хорошо сжимается: объект части хранится в zstd.
        filler = "The quick brown fox jumps over the lazy dog.\n".repeat(40),
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
    // Письмо хранится сжатым (MS-15, `blob.compression` по умолчанию), чтение распаковывает.
    let mut tx = tp_meta::schema::begin(&pool, tenant).await.expect("begin");
    let zstd: i64 = sqlx::query_scalar("SELECT count(*) FROM blobs WHERE compression = 'zstd'")
        .fetch_one(&mut *tx)
        .await
        .expect("blobs");
    tx.commit().await.expect("commit");
    assert!(zstd >= 1, "нет сжатых объектов");
    // Журнал доставки (SMTP-17): приём и доставка письма в ящик.
    let query = DeliveryQuery {
        message_id: Some(format!("e2e-{word}@sender.test")),
        ..DeliveryQuery::all(10)
    };
    let until = deadline();
    loop {
        let log = DeliveryLog::search(&meta, tenant, &query)
            .await
            .expect("delivery log");
        let kinds: Vec<_> = log.events.iter().map(|e| e.kind).collect();
        if kinds.contains(&DeliveryEventKind::Receive)
            && kinds.contains(&DeliveryEventKind::Deliver)
        {
            break;
        }
        retry(until, "приём и доставка в журнале доставки").await;
    }

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

    // 3а. Календари и контакты (APP-29): роль dav на слушателе JMAP, вход паролем приложения.
    let basic = format!("Basic {}", jmap.auth);
    let web = addr(&running, "jmap");
    let found = dav(web, "GET", "/.well-known/caldav", "", "0").await;
    assert!(found.starts_with("HTTP/1.1 301"), "{found}");
    assert!(found.to_ascii_lowercase().contains("location: "), "{found}");
    assert!(found.contains("/dav/"), "{found}");
    let home = format!("/dav/calendars/{login}/");
    let denied = dav(web, "PROPFIND", &home, "Basic bm9ib2R5Om5vcGU=", "1").await;
    assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");
    let listed = dav(web, "PROPFIND", &home, &basic, "1").await;
    assert!(listed.starts_with("HTTP/1.1 207"), "{listed}");
    // Дом и календарь по умолчанию (создаётся при первом обращении).
    assert!(listed.matches("<d:response>").count() >= 2, "{listed}");
    let books = format!("/dav/addressbooks/{login}/");
    let listed = dav(web, "PROPFIND", &books, &basic, "1").await;
    assert!(listed.starts_with("HTTP/1.1 207"), "{listed}");

    // 3б. Событие из JMAP видно в CalDAV (APP-27): одно хранилище календарей.
    let (code, body) = http(web, "GET", "/jmap/session", &basic, None).await;
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    let session: Value = serde_json::from_slice(&body).expect("session");
    for capability in [
        "urn:ietf:params:jmap:contacts",
        "urn:ietf:params:jmap:calendars",
        "urn:ietf:params:jmap:sieve",
        "urn:ietf:params:jmap:vacationresponse",
    ] {
        assert!(
            session["capabilities"][capability].is_object(),
            "{capability}: {session}"
        );
    }
    let set = jmap
        .call(
            "CalendarEvent/set",
            json!({ "accountId": jmap.account, "create": { "e": {
                "uid": "app-27-e2e", "title": "Planning e2e",
                "start": "2026-12-01T10:00:00", "timeZone": "Europe/Moscow", "duration": "PT1H",
            } } }),
        )
        .await;
    assert!(set["created"]["e"]["id"].is_string(), "{set}");
    let hrefs = |listing: &str| -> Vec<String> {
        listing
            .split("<d:href>")
            .skip(1)
            .filter_map(|r| r.split("</d:href>").next().map(str::to_owned))
            .collect()
    };
    let listed = dav(web, "PROPFIND", &home, &basic, "1").await;
    let mut event = None;
    for calendar in hrefs(&listed).into_iter().filter(|h| *h != home) {
        let objects = dav(web, "PROPFIND", &calendar, &basic, "1").await;
        if let Some(href) = hrefs(&objects).into_iter().find(|h| h.ends_with(".ics")) {
            event = Some(href);
        }
    }
    let event = event.unwrap_or_else(|| panic!("события нет в CalDAV: {listed}"));
    let ics = dav(web, "GET", &event, &basic, "0").await;
    assert!(ics.starts_with("HTTP/1.1 200"), "{ics}");
    assert!(ics.contains("UID:app-27-e2e"), "{ics}");
    assert!(ics.contains("SUMMARY:Planning e2e"), "{ics}");

    // 3в. ManageSieve в роли imap (APP-27): STARTTLS, вход PLAIN (служба SASL `sieve`), LISTSCRIPTS.
    let mut tcp = TcpStream::connect(addr(&running, "managesieve"))
        .await
        .expect("managesieve connect");
    let greeting = sieve_reply(&mut tcp).await;
    assert!(sieve_ok(&greeting), "{greeting}");
    assert!(greeting.contains("\"STARTTLS\""), "{greeting}");
    assert!(sieve_ok(&sieve_cmd(&mut tcp, "STARTTLS").await));
    let name = ServerName::try_from("localhost").expect("name");
    let mut sieve = tls_connector()
        .connect(name, tcp)
        .await
        .expect("managesieve tls");
    let capabilities = sieve_reply(&mut sieve).await;
    assert!(sieve_ok(&capabilities), "{capabilities}");
    let sasl = capabilities
        .split("\r\n")
        .find(|l| l.starts_with("\"SASL\""))
        .unwrap_or_else(|| panic!("нет SASL: {capabilities}"));
    assert!(sasl.contains("PLAIN"), "{sasl}");
    assert!(sasl.contains("OAUTHBEARER"), "{sasl}");
    let b64 = base64::engine::general_purpose::STANDARD;
    let wrong = b64.encode(format!("\0{login}\0wrong-password"));
    let denied = sieve_cmd(&mut sieve, &format!("AUTHENTICATE \"PLAIN\" \"{wrong}\"")).await;
    assert!(denied.starts_with("NO"), "{denied}");
    let plain = b64.encode(format!("\0{login}\0{app_password}"));
    let entered = sieve_cmd(&mut sieve, &format!("AUTHENTICATE \"PLAIN\" \"{plain}\"")).await;
    assert!(sieve_ok(&entered), "{entered}");
    let script = "require \"fileinto\";\r\nfileinto \"INBOX\";\r\n";
    let put = sieve_cmd(
        &mut sieve,
        &format!("PUTSCRIPT \"e2e\" {{{}+}}\r\n{script}", script.len()),
    )
    .await;
    assert!(sieve_ok(&put), "{put}");
    let scripts = sieve_cmd(&mut sieve, "LISTSCRIPTS").await;
    assert!(sieve_ok(&scripts), "{scripts}");
    assert!(scripts.contains("\"e2e\""), "{scripts}");
    assert!(sieve_ok(
        &sieve_cmd(&mut sieve, "DELETESCRIPT \"e2e\"").await
    ));
    let bye = sieve_cmd(&mut sieve, "LOGOUT").await;
    assert!(sieve_ok(&bye), "{bye}");

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

    // 6a. «Очистить корзину» в IMAP (EXPUNGE в корзине; решение 2026-11-01-trash-purge): по умолчанию `hide` —
    // письмо исчезает у пользователя и хранится скрыто до конца срока корзины.
    imap.cmd("STORE 1 +FLAGS.SILENT (\\Deleted)").await;
    imap.cmd("EXPUNGE").await;
    assert_eq!(exists(&imap.cmd("SELECT Trash").await), 1);
    imap.cmd("STORE 1 +FLAGS.SILENT (\\Deleted)").await;
    imap.cmd("EXPUNGE").await;
    assert_eq!(exists(&imap.cmd("SELECT Trash").await), 0);
    let visible = TrashStore::list(&meta, scope, 0, 10).await.expect("trash");
    assert!(visible.is_empty(), "{visible:?}");
    let hidden = TrashStore::list_hidden(&meta, scope, 0, 10)
        .await
        .expect("hidden");
    assert_eq!(hidden.len(), 1, "{hidden:?}");
    assert_eq!(hidden[0].object, TrashObject::Message(id));

    // 6b. Администратор задаёт `trash.purge: erase`; скрытое письмо восстановлено. «Очистить корзину» в JMAP
    // (`Email/set destroy` письма в корзине) стирает его сразу.
    let erase = TrashPurgeRule {
        mode: TrashPurge::Erase,
        user_can_choose: false,
    };
    meta.set_policy(
        tenant,
        tp_meta::retention::PolicyTarget::Tenant,
        &Policy {
            trash_purge: Some(erase),
            ..Policy::default()
        },
    )
    .await
    .expect("policy");
    TrashStore::restore(&meta, scope, TrashObject::Message(id))
        .await
        .expect("restore hidden");
    assert_eq!(jmap.query(json!({ "inMailbox": inbox })).await, ids);
    let trash_box = jmap.mailbox("trash").await;
    let destroy = json!({ "accountId": jmap.account, "destroy": ids });
    let r = jmap.call("Email/set", destroy.clone()).await;
    assert_eq!(r["destroyed"], json!(ids), "{r}");
    assert_eq!(jmap.query(json!({ "inMailbox": trash_box })).await, ids);
    let r = jmap.call("Email/set", destroy).await;
    assert_eq!(r["destroyed"], json!(ids), "{r}");
    assert!(
        jmap.query(json!({ "inMailbox": trash_box }))
            .await
            .is_empty()
    );
    let hidden = TrashStore::list_hidden(&meta, scope, 0, 10)
        .await
        .expect("hidden");
    assert!(hidden.is_empty(), "{hidden:?}");
    assert!(
        MessageStore::get(&meta, scope, id)
            .await
            .expect("get")
            .is_none()
    );
    imap.cmd("LOGOUT").await;

    // 7. Отправка из JMAP локальному адресату (CMP-07): письмо сразу во «Входящих» получателя — без DNS и очереди.
    let bob_login = format!("bob@{domain}");
    let bob = meta
        .upsert(
            tenant,
            AccountUpsert {
                domain: domain_id,
                login: bob_login.clone(),
                display_name: "Bob".into(),
                external_id: None,
                enabled: true,
                aliases: Vec::new(),
            },
        )
        .await
        .expect("bob");
    let bob_scope = AccountScope {
        tenant,
        account: bob.id,
    };
    let (_, bob_secret) = apps.issue(bob_scope, "e2e").await.expect("issue bob");
    let identities = jmap
        .call("Identity/get", json!({ "accountId": jmap.account }))
        .await;
    let identity = identities["list"][0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("нет идентичности: {identities}"))
        .to_owned();
    let drafts = jmap.mailbox("drafts").await;
    let sent_subject = "JMAP local submission";
    let created = jmap
        .call(
            "Email/set",
            json!({
                "accountId": jmap.account,
                "create": { "m": {
                    "mailboxIds": { drafts: true },
                    "from": [{ "name": "Alice", "email": login }],
                    "to": [{ "name": "Bob", "email": bob_login }],
                    "subject": sent_subject,
                    "textBody": [{ "partId": "t", "type": "text/plain" }],
                    "bodyValues": { "t": { "value": "Hello from JMAP.\n" } },
                } },
            }),
        )
        .await;
    let email_id = created["created"]["m"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("черновик не создан: {created}"))
        .to_owned();
    let submitted = jmap
        .call(
            "EmailSubmission/set",
            json!({
                "accountId": jmap.account,
                "create": { "s": { "identityId": identity, "emailId": email_id } },
            }),
        )
        .await;
    let status = &submitted["created"]["s"]["deliveryStatus"][&bob_login];
    assert_eq!(status["delivered"], "yes", "{submitted}");
    let bob_jmap = Jmap {
        addr: jmap.addr,
        auth: base64::engine::general_purpose::STANDARD
            .encode(format!("{bob_login}:{}", bob_secret.expose())),
        account: bob.id.to_string(),
    };
    let bob_inbox = bob_jmap.mailbox("inbox").await;
    let bob_ids = bob_jmap.query(json!({ "inMailbox": bob_inbox })).await;
    assert_eq!(bob_ids.len(), 1, "{bob_ids:?}");
    let got = bob_jmap
        .call(
            "Email/get",
            json!({ "accountId": bob_jmap.account, "ids": bob_ids, "properties": ["subject", "from"] }),
        )
        .await;
    assert_eq!(got["list"][0]["subject"], sent_subject, "{got}");
    assert_eq!(got["list"][0]["from"][0]["email"], login, "{got}");

    // 8. Истечение срока корзины (роль maintenance, таймер на ближайший срок): срок организации 2 с — письмо,
    // удалённое в корзину, окончательно удаляет воркер истечения узла.
    meta.set_policy(
        tenant,
        tp_meta::retention::PolicyTarget::Tenant,
        &Policy {
            trash_purge: Some(erase),
            trash_retention: Some(Duration::from_secs(2)),
            ..Policy::default()
        },
    )
    .await
    .expect("policy");
    let r = bob_jmap
        .call(
            "Email/set",
            json!({ "accountId": bob_jmap.account, "destroy": bob_ids }),
        )
        .await;
    assert_eq!(r["destroyed"], json!(bob_ids), "{r}");
    let trashed = TrashStore::list(&meta, bob_scope, 0, 10)
        .await
        .expect("trash");
    assert!(
        trashed
            .iter()
            .all(|t| (t.expires_at - t.deleted_at).whole_seconds() == 2),
        "{trashed:?}"
    );
    let until = deadline();
    loop {
        let visible = TrashStore::list(&meta, bob_scope, 0, 10)
            .await
            .expect("trash");
        let hidden = TrashStore::list_hidden(&meta, bob_scope, 0, 10)
            .await
            .expect("hidden");
        if visible.is_empty() && hidden.is_empty() {
            break;
        }
        retry(until, "истечение срока корзины (maintenance)").await;
    }

    shutdown.cancel();
    let report = running.wait().await.expect("wait");
    assert!(report.failed.is_empty(), "{report:?}");
    pool.close().await;
    drop_database(&url, &db_name).await;
}

// ---------- настройки транспорта и правила потока почты (APP-09) ----------

/// Вызовы правил потока почты тестового модуля.
static FLOW_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Правила потока почты: письмо без изменений, вызов учтён.
#[derive(Debug)]
struct CountingFlow;

#[async_trait]
impl MailFlow for CountingFlow {
    async fn apply(&self, input: &FlowInput) -> tp_contracts::Result<FlowOutcome> {
        FLOW_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(FlowOutcome::unchanged(input))
    }
}

/// Модуль теста: регистрирует [`CountingFlow`] в `Registry`.
#[derive(Debug)]
struct FlowModule;

impl Module for FlowModule {
    fn name(&self) -> &str {
        "flowcount"
    }

    fn migrations(&self) -> &[Migration] {
        &[]
    }

    fn register(&self, registry: &mut Registry, _ctx: &ModuleCtx) -> tp_contracts::Result<()> {
        registry.set_mail_flow(Arc::new(CountingFlow))
    }
}

const FLOW_MODULES: &[Builtin] = &[Builtin {
    name: "flowcount",
    make: || Arc::new(FlowModule),
}];

/// Конфигурация [`config`] со слушателем Admin API и локальным сокетом `admin init`.
fn admin_config(dir: &Path, db_url: &str, domain: &str, socket: &Path) -> String {
    let jmap = "http:\n  jmap: { listen: \"127.0.0.1:0\", allow_plaintext: true }\n";
    let base = config(dir, db_url, domain);
    assert!(base.contains(jmap), "{base}");
    let base = base.replace(
        jmap,
        &format!("{jmap}  admin: {{ listen: \"127.0.0.1:0\", allow_plaintext: true }}\n"),
    );
    format!(
        "{base}admin:\n  init:\n    socket: \"{socket}\"\n",
        socket = socket.display().to_string().replace('\\', "/"),
    )
}

/// [`admin_config`] с модулем `flowcount`.
fn transport_config(dir: &Path, db_url: &str, domain: &str, socket: &Path) -> String {
    format!(
        "{}modules:\n  flowcount:\n    enabled: true\n",
        admin_config(dir, db_url, domain, socket)
    )
}

/// Сокет `admin init` роль admin забирает из общего для процесса места (`local_admin::bind` → `local_admin::serve`
/// при запуске узла): запуски узлов в тестах — по очереди, иначе сокет достаётся чужому узлу.
static ADMIN_SOCKET: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// Новая сессия SMTP: код ответа на `MAIL FROM:<from>`.
async fn mail_from_code(addr: std::net::SocketAddr, from: &str) -> u16 {
    let mut s = TcpStream::connect(addr).await.expect("smtp connect");
    assert_eq!(smtp_reply(&mut s).await, 220);
    assert_eq!(smtp_cmd(&mut s, "EHLO app.test").await, 250);
    let code = smtp_cmd(&mut s, &format!("MAIL FROM:<{from}>")).await;
    assert_eq!(smtp_cmd(&mut s, "QUIT").await, 221);
    code
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transport_settings_without_restart() {
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

    let domain = "example.ru".to_owned();
    let socket = dir.path().join("admin.sock");
    let path = dir.path().join("config.yaml");
    std::fs::write(
        &path,
        transport_config(dir.path(), &db_url, &domain, &socket),
    )
    .expect("config");
    // Локальный сокет `admin init` открывается до запуска узла (как `startup::prepare_serve`).
    let cfg = thepost::sections::registry_of(FLOW_MODULES)
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
        modules: FLOW_MODULES,
    })
    .await
    .expect("start");
    let until = deadline();
    while !running.health.is_ready() {
        retry(until, "узел готов").await;
    }
    let smtp = addr(&running, "smtp-in");
    let admin = addr(&running, "admin");

    // Первый администратор: учётная запись, принятый домен и одноразовый токен Admin API.
    let login = format!("admin@{domain}");
    let granted = admin_init(&socket, &login).await;
    drop(bound);
    assert_eq!(granted["status"], "granted", "{granted}");
    let token = granted["token"].as_str().expect("token").to_owned();

    // До источника: соединение с 127.0.0.1 — обычный клиент, отправитель любой (снимок настроек прочитан).
    assert_eq!(mail_from_code(smtp, "robot@other.test").await, 250);

    // Admin API: сессия, версия снимка кластера, источник-приложение организации для 127.0.0.1.
    let (code, body) = http(
        admin,
        "POST",
        "/api/v1/session",
        &format!("Bearer {token}"),
        None,
    )
    .await;
    assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
    let session: Value = serde_json::from_slice(&body).expect("session");
    let bearer = format!("Bearer {}", session["session"].as_str().expect("session"));
    let (code, body) = http(admin, "GET", "/api/v1/transport", &bearer, None).await;
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    let cluster: Value = serde_json::from_slice(&body).expect("cluster");
    let version = cluster["version"].as_u64().expect("version");
    let source = json!({
        "version": version,
        "matcher": { "networks": ["127.0.0.1/32"] },
        "kind": { "application": { "senders": ["@allowed.test"], "external": false } },
    });
    let (code, body) = http(
        admin,
        "PUT",
        &format!("/api/v1/orgs/{}/transport/sources/app", TenantId::DEFAULT),
        &bearer,
        Some(&source),
    )
    .await;
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));

    // ADM-34: политика организации — через контракт `PolicyStore` в PostgreSQL; изменения — в журнале аудита
    // (`audit_log`), `GET …/audit` отдаёт их.
    let org = format!("/api/v1/orgs/{}", TenantId::DEFAULT);
    let policy = json!({ "max_recipients": 7 });
    let (code, body) = http(
        admin,
        "PUT",
        &format!("{org}/policy"),
        &bearer,
        Some(&policy),
    )
    .await;
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    let (code, body) = http(admin, "GET", &org, &bearer, None).await;
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    let read: Value = serde_json::from_slice(&body).expect("org");
    assert_eq!(read["policy"]["max_recipients"], 7, "{read}");
    let (code, body) = http(admin, "GET", &format!("{org}/audit"), &bearer, None).await;
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    let audit: Value = serde_json::from_slice(&body).expect("audit");
    let changes: Vec<(&str, &str, u64)> = audit["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter_map(|e| {
            Some((
                e["method"].as_str()?,
                e["route"].as_str()?,
                e["status"].as_u64()?,
            ))
        })
        .collect();
    for change in [
        ("PUT", "/api/v1/orgs/{org}/transport/sources/{name}", 200),
        ("PUT", "/api/v1/orgs/{org}/policy", 200),
    ] {
        assert!(changes.contains(&change), "{change:?}: {audit}");
    }

    // Следующая сессия без перезапуска (сигнал `TransportChanged`): отправитель вне списка источника — отказ.
    let until = deadline();
    while mail_from_code(smtp, "robot@other.test").await != 550 {
        retry(until, "доверенный источник действует на новую сессию").await;
    }
    assert_eq!(mail_from_code(smtp, "robot@allowed.test").await, 250);

    // Письмо от приложения: правила потока почты модуля — ровно один вызов.
    assert_eq!(FLOW_CALLS.load(Ordering::SeqCst), 0);
    let message = format!(
        "From: Robot <robot@allowed.test>\nTo: <{login}>\nSubject: transport settings\n\
         Message-ID: <app09-{}@allowed.test>\n\nScanned document.\n",
        std::process::id()
    );
    smtp_send(smtp, "robot@allowed.test", &login, &message).await;
    assert_eq!(FLOW_CALLS.load(Ordering::SeqCst), 1);

    shutdown.cancel();
    let report = running.wait().await.expect("wait");
    assert!(report.failed.is_empty(), "{report:?}");
    drop_database(&url, &db_name).await;
}

// ---------- APP-26: правила Sieve и автоответ ----------

/// Получатели очереди исходящей почты с адресом `address`.
/// Пул `tp-meta` к базе `url` без учёта ожидания соединения (наблюдатель-пустышка): метрик в тесте нет.
async fn db_pool(url: &str) -> tp_meta::pool::DbPool {
    let pool = sqlx::PgPool::connect(url).await.expect("pool");
    tp_meta::pool::DbPool::new(pool, Arc::new(tp_meta::pool::NoAcquireObserver))
}

async fn queued(pool: &tp_meta::pool::DbPool, address: &str) -> i64 {
    let mut tx = tp_meta::schema::begin(pool, TenantId::DEFAULT)
        .await
        .expect("begin");
    sqlx::query_scalar("SELECT count(*) FROM queue_rcpts WHERE address = $1")
        .bind(address)
        .fetch_one(&mut *tx)
        .await
        .expect("queue")
}

/// Скрипт и автоответ через Admin API → письмо по SMTP → папка по `fileinto` и автоответ в очереди (один на
/// отправителя за период); автоответ виден в JMAP `VacationResponse/get` (Postman).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sieve_rules_and_vacation() {
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

    let domain = "example.ru".to_owned();
    let socket = dir.path().join("admin.sock");
    let path = dir.path().join("config.yaml");
    // Без роли smtp-out: автоответ остаётся в очереди, его никто не забирает.
    let text = admin_config(dir.path(), &db_url, &domain, &socket).replace(
        "roles: [all]",
        "roles: [smtp-in, submission, imap, jmap, admin, search, indexer, maintenance]",
    );
    assert!(!text.contains("roles: [all]"), "{text}");
    std::fs::write(&path, text).expect("config");
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
    let until = deadline();
    while !running.health.is_ready() {
        retry(until, "узел готов").await;
    }
    let smtp = addr(&running, "smtp-in");
    let admin = addr(&running, "admin");

    // Администратор — он же получатель: учётная запись и принятый домен заведены `admin init`.
    let login = format!("admin@{domain}");
    let granted = admin_init(&socket, &login).await;
    drop(bound);
    assert_eq!(granted["status"], "granted", "{granted}");
    let token = granted["token"].as_str().expect("token").to_owned();
    let (code, body) = http(
        admin,
        "POST",
        "/api/v1/session",
        &format!("Bearer {token}"),
        None,
    )
    .await;
    assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
    let session: Value = serde_json::from_slice(&body).expect("session");
    let bearer = format!("Bearer {}", session["session"].as_str().expect("session"));

    let pool = db_pool(&url_db).await;
    let tenant = TenantId::DEFAULT;
    let mut tx = tp_meta::schema::begin(&pool, tenant).await.expect("begin");
    let account: sqlx::types::Uuid =
        sqlx::query_scalar("SELECT account_id FROM accounts WHERE login = $1")
            .bind(&login)
            .fetch_one(&mut *tx)
            .await
            .expect("account");
    drop(tx);
    let base = format!("/api/v1/orgs/{tenant}/accounts/{account}");

    // Admin API: неверный скрипт — 422 с позицией; верный — записан и включён.
    let script = format!("{base}/sieve/scripts/main");
    let (code, body) = http(
        admin,
        "PUT",
        &script,
        &bearer,
        Some(&json!({ "text": "if header :contains \"subject\" {" })),
    )
    .await;
    assert_eq!(code, 422, "{}", String::from_utf8_lossy(&body));
    let problem: Value = serde_json::from_slice(&body).expect("problem");
    assert!(problem["errors"][0]["line"].is_u64(), "{problem}");
    let rule = "require [\"fileinto\", \"mailbox\"];\n\
                if header :contains \"subject\" \"report\" { fileinto :create \"Reports\"; }\n";
    let (code, body) = http(
        admin,
        "PUT",
        &script,
        &bearer,
        Some(&json!({ "text": rule })),
    )
    .await;
    assert!(
        matches!(code, 200 | 201 | 204),
        "{code} {}",
        String::from_utf8_lossy(&body)
    );
    let (code, body) = http(admin, "POST", &format!("{script}/activate"), &bearer, None).await;
    assert!(
        matches!(code, 200 | 204),
        "{code} {}",
        String::from_utf8_lossy(&body)
    );

    // Admin API: автоответ за пользователя.
    let (code, body) = http(
        admin,
        "PUT",
        &format!("{base}/vacation"),
        &bearer,
        Some(
            &json!({ "enabled": true, "subject": "Out of office", "text_body": "Back on Monday." }),
        ),
    )
    .await;
    assert!(
        matches!(code, 200 | 204),
        "{code} {}",
        String::from_utf8_lossy(&body)
    );

    // JMAP (Postman): тот же автоответ в `VacationResponse/get`.
    let meta = PgMeta::new(pool.clone(), MetaConfig::default());
    let scope = AccountScope {
        tenant,
        account: tp_contracts::ids::AccountId::from_uuid(account),
    };
    let apps =
        tp_auth::AppPasswords::new(Arc::new(meta.clone()), tp_auth::PasswordConfig::default())
            .expect("app passwords");
    let (_, secret) = apps.issue(scope, "e2e").await.expect("issue");
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{login}:{}", secret.expose()))
    );
    let jmap = addr(&running, "jmap");
    let jmap_call = |method: &'static str, args: Value| {
        let basic = basic.clone();
        async move {
            let req = json!({
                "using": [
                    "urn:ietf:params:jmap:core",
                    "urn:ietf:params:jmap:mail",
                    "urn:ietf:params:jmap:vacationresponse",
                ],
                "methodCalls": [[method, args, "c0"]],
            });
            let (code, body) = http(jmap, "POST", "/jmap/api", &basic, Some(&req)).await;
            assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
            let resp: Value = serde_json::from_slice(&body).expect("json");
            assert_eq!(resp["methodResponses"][0][0], method, "{resp}");
            resp["methodResponses"][0][1].clone()
        }
    };
    let account_id = account.to_string();
    let r = jmap_call("VacationResponse/get", json!({ "accountId": account_id })).await;
    assert_eq!(r["list"][0]["isEnabled"], true, "{r}");
    assert_eq!(r["list"][0]["textBody"], "Back on Monday.", "{r}");

    // Письмо внешнего отправителя: в папку по правилу, автоответ — в очередь.
    let sender = "bob@sender.test";
    let send = |n: u32, subject: &'static str| {
        let message = format!(
            "From: Bob <{sender}>\nTo: <{login}>\nSubject: {subject}\n\
             Message-ID: <app26-{n}-{}@sender.test>\n\nSee attached.\n",
            std::process::id()
        );
        let login = login.clone();
        async move { smtp_send(smtp, sender, &login, &message).await }
    };
    send(1, "Weekly report").await;
    let until = deadline();
    let reports = loop {
        let r = jmap_call("Mailbox/get", json!({ "accountId": account_id })).await;
        let found = r["list"]
            .as_array()
            .and_then(|l| l.iter().find(|m| m["name"] == "Reports"))
            .cloned();
        match found {
            Some(m) if m["totalEmails"] == 1 => break m,
            _ => retry(until, "письмо в папке Reports (fileinto)").await,
        }
    };
    assert_eq!(reports["totalEmails"], 1, "{reports}");
    let until = deadline();
    while queued(&pool, sender).await != 1 {
        retry(until, "автоответ в очереди").await;
    }

    // Второе письмо того же отправителя: мимо правила — во «Входящие», повторного автоответа нет.
    send(2, "Lunch").await;
    let until = deadline();
    loop {
        let r = jmap_call("Mailbox/get", json!({ "accountId": account_id })).await;
        let inbox = r["list"]
            .as_array()
            .and_then(|l| l.iter().find(|m| m["role"] == "inbox"))
            .cloned();
        if inbox.is_some_and(|m| m["totalEmails"] == 1) {
            break;
        }
        retry(until, "второе письмо во «Входящих»").await;
    }
    assert_eq!(queued(&pool, sender).await, 1);

    shutdown.cancel();
    let report = running.wait().await.expect("wait");
    assert!(report.failed.is_empty(), "{report:?}");
    pool.close().await;
    drop_database(&url, &db_name).await;
}

// ---------- APP-28: планирование встреч ----------

/// Запрос DAV с телом iCalendar и дополнительными заголовками `headers` (строки с `\r\n`): ответ целиком.
async fn dav_send(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    authorization: &str,
    headers: &str,
    ics: &str,
) -> String {
    let mut s = TcpStream::connect(addr).await.expect("dav connect");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: {authorization}\r\n\
         Content-Type: text/calendar; charset=utf-8\r\n{headers}\
         Content-Length: {}\r\nConnection: close\r\n\r\n{ics}",
        ics.len()
    );
    s.write_all(req.as_bytes()).await.expect("dav write");
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.expect("dav read");
    String::from_utf8_lossy(&raw).into_owned()
}

/// Адреса ответа `PROPFIND`.
fn dav_hrefs(listing: &str) -> Vec<String> {
    listing
        .split("<d:href>")
        .skip(1)
        .filter_map(|r| r.split("</d:href>").next().map(str::to_owned))
        .collect()
}

/// Календари дома `home` (без самого дома, входящих и исходящих планирования).
async fn dav_calendars(web: std::net::SocketAddr, home: &str, basic: &str) -> Vec<String> {
    let listed = dav(web, "PROPFIND", home, basic, "1").await;
    assert!(listed.starts_with("HTTP/1.1 207"), "{listed}");
    dav_hrefs(&listed)
        .into_iter()
        .filter(|h| h != home && !h.ends_with("/inbox/") && !h.ends_with("/outbox/"))
        .collect()
}

/// Объект календаря с `UID:{uid}` в доме `home`: текст iCalendar.
async fn dav_event(
    web: std::net::SocketAddr,
    home: &str,
    basic: &str,
    uid: &str,
) -> Option<String> {
    for calendar in dav_calendars(web, home, basic).await {
        let objects = dav(web, "PROPFIND", &calendar, basic, "1").await;
        for href in dav_hrefs(&objects)
            .into_iter()
            .filter(|h| h.ends_with(".ics"))
        {
            let ics = dav(web, "GET", &href, basic, "0").await;
            if ics.contains(&format!("UID:{uid}")) {
                return Some(ics);
            }
        }
    }
    None
}

/// Планирование встреч в узле (APP-28): организатор создаёт встречу в CalDAV (`PUT`) и в JMAP
/// (`sendSchedulingMessages`) — участник организации получает событие в свой календарь, внешнему участнику письмо
/// iMIP ставится в очередь исходящей почты; `POST` в исходящие планирования разрешён.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn meeting_scheduling() {
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

    let domain = "example.ru".to_owned();
    let path = dir.path().join("config.yaml");
    // Без роли smtp-out: письма iMIP остаются в очереди, их никто не забирает. Без роли admin — сокет
    // `admin init` не нужен.
    let text = config(dir.path(), &db_url, &domain).replace(
        "roles: [all]",
        "roles: [smtp-in, imap, jmap, dav, search, indexer, maintenance]",
    );
    assert!(!text.contains("roles: [all]"), "{text}");
    std::fs::write(&path, text).expect("config");
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
    let until = deadline();
    while !running.health.is_ready() {
        retry(until, "узел готов").await;
    }

    let pool = db_pool(&url_db).await;
    let meta = PgMeta::new(pool.clone(), MetaConfig::default());
    let tenant = TenantId::DEFAULT;
    let domain_id = accept_domain(&meta, tenant, &domain).await;
    let apps =
        tp_auth::AppPasswords::new(Arc::new(meta.clone()), tp_auth::PasswordConfig::default())
            .expect("app passwords");
    let mut users = Vec::new();
    for name in ["alice", "bob"] {
        let login = format!("{name}@{domain}");
        let account = meta
            .upsert(
                tenant,
                AccountUpsert {
                    domain: domain_id,
                    login: login.clone(),
                    display_name: name.to_owned(),
                    external_id: None,
                    enabled: true,
                    aliases: Vec::new(),
                },
            )
            .await
            .expect("account");
        let scope = AccountScope {
            tenant,
            account: account.id,
        };
        let (_, secret) = apps.issue(scope, "e2e").await.expect("issue");
        let auth = base64::engine::general_purpose::STANDARD
            .encode(format!("{login}:{}", secret.expose()));
        users.push((login, account.id.to_string(), auth));
    }
    let (alice, alice_id, alice_auth) = users[0].clone();
    let (bob, _, bob_auth) = users[1].clone();
    let web = addr(&running, "jmap");
    let alice_basic = format!("Basic {alice_auth}");
    let bob_basic = format!("Basic {bob_auth}");
    let alice_home = format!("/dav/calendars/{alice}/");
    let bob_home = format!("/dav/calendars/{bob}/");
    let external = "carol@external.test";
    let external_jmap = "dave@external.test";

    // 1. CalDAV: организатор кладёт встречу в свой календарь — сервер рассылает приглашения сам.
    let calendars = dav_calendars(web, &alice_home, &alice_basic).await;
    let calendar = calendars
        .first()
        .unwrap_or_else(|| panic!("нет календаря по умолчанию: {calendars:?}"));
    let ics = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//thepost//e2e//EN\r\nBEGIN:VEVENT\r\n\
         UID:app-28-dav\r\nDTSTAMP:20261112T100000Z\r\nDTSTART:20270111T070000Z\r\n\
         DTEND:20270111T080000Z\r\nSEQUENCE:0\r\nSUMMARY:Planning DAV\r\n\
         ORGANIZER:mailto:{alice}\r\n\
         ATTENDEE;PARTSTAT=ACCEPTED:mailto:{alice}\r\n\
         ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:{bob}\r\n\
         ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:{external}\r\n\
         END:VEVENT\r\nEND:VCALENDAR\r\n"
    );
    let put = dav_send(
        web,
        "PUT",
        &format!("{calendar}app-28-dav.ics"),
        &alice_basic,
        "If-None-Match: *\r\n",
        &ics,
    )
    .await;
    assert!(put.starts_with("HTTP/1.1 201"), "{put}");
    // Участник организации: событие в его календаре, ответа ещё нет.
    let invited = dav_event(web, &bob_home, &bob_basic, "app-28-dav")
        .await
        .expect("приглашение в календаре участника");
    assert!(invited.contains("SUMMARY:Planning DAV"), "{invited}");
    assert!(invited.contains("NEEDS-ACTION"), "{invited}");
    // Внешний участник: письмо iMIP в очереди исходящей почты; участнику организации письмо не ставится.
    assert_eq!(queued(&pool, external).await, 1);
    assert_eq!(queued(&pool, &bob).await, 0);
    // Итог рассылки записан в объект организатора (`SCHEDULE-STATUS`).
    let mine = dav_event(web, &alice_home, &alice_basic, "app-28-dav")
        .await
        .expect("событие организатора");
    assert!(mine.contains("SCHEDULE-STATUS"), "{mine}");

    // 2. Занятость (`POST` VFREEBUSY в исходящие планирования; без планирования — 403): у организатора занято
    // время встречи, о внешнем адресе сведений нет (`3.7`).
    let request = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//thepost//e2e//EN\r\nMETHOD:REQUEST\r\n\
         BEGIN:VFREEBUSY\r\nUID:app-28-fb\r\nDTSTAMP:20261112T100000Z\r\n\
         DTSTART:20270111T000000Z\r\nDTEND:20270112T000000Z\r\nORGANIZER:mailto:{bob}\r\n\
         ATTENDEE:mailto:{alice}\r\nATTENDEE:mailto:{external}\r\n\
         END:VFREEBUSY\r\nEND:VCALENDAR\r\n"
    );
    let busy = dav_send(
        web,
        "POST",
        &format!("{bob_home}outbox/"),
        &bob_basic,
        "",
        &request,
    )
    .await;
    assert!(busy.starts_with("HTTP/1.1 200"), "{busy}");
    assert!(busy.contains("schedule-response"), "{busy}");
    assert!(busy.contains("20270111T070000Z/20270111T080000Z"), "{busy}");
    assert!(busy.contains("3.7"), "{busy}");

    // 3. JMAP: `sendSchedulingMessages` — те же получатели тем же путём.
    let jmap = Jmap {
        addr: web,
        auth: alice_auth,
        account: alice_id,
    };
    let set = jmap
        .call(
            "CalendarEvent/set",
            json!({ "accountId": jmap.account, "sendSchedulingMessages": true, "create": { "e": {
                "uid": "app-28-jmap", "title": "Planning JMAP",
                "start": "2027-01-12T10:00:00", "timeZone": "Europe/Moscow", "duration": "PT1H",
                "organizerCalendarAddress": format!("mailto:{alice}"),
                "participants": {
                    "o": { "@type": "Participant", "name": "Alice",
                           "calendarAddress": format!("mailto:{alice}"), "roles": { "owner": true } },
                    "b": { "@type": "Participant", "name": "Bob",
                           "calendarAddress": format!("mailto:{bob}"), "roles": { "attendee": true },
                           "participationStatus": "needs-action", "expectReply": true },
                    "x": { "@type": "Participant", "name": "Dave",
                           "calendarAddress": format!("mailto:{external_jmap}"),
                           "roles": { "attendee": true },
                           "participationStatus": "needs-action", "expectReply": true },
                },
            } } }),
        )
        .await;
    assert!(set["created"]["e"]["id"].is_string(), "{set}");
    let invited = dav_event(web, &bob_home, &bob_basic, "app-28-jmap")
        .await
        .expect("приглашение из JMAP в календаре участника");
    assert!(invited.contains("SUMMARY:Planning JMAP"), "{invited}");
    assert_eq!(queued(&pool, external_jmap).await, 1);
    assert_eq!(queued(&pool, &bob).await, 0);
    pool.close().await;
    shutdown.cancel();
    let report = running.wait().await.expect("report");
    assert!(report.failed.is_empty(), "{report:?}");
    drop_database(&url, &db_name).await;
}

// ---------- APP-34: метрики Д.7 ----------

/// `/metrics` узла с шиной LISTEN/NOTIFY содержит показатели критериев Д.7: пул СУБД, слушатель, время фиксаций
/// с NOTIFY и число слушателей, «сироты» BLOB.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn d7_metrics() {
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

    let path = dir.path().join("config.yaml");
    let text = config(dir.path(), &db_url, "example.ru")
        .replace("roles: [all]", "roles: [jmap, maintenance]");
    assert!(!text.contains("roles: [all]"), "{text}");
    assert!(!text.contains("\nbus:"), "{text}");
    std::fs::write(&path, format!("{text}bus:\n  kind: postgres\n")).expect("config");
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
    let until = deadline();
    while !running.health.is_ready() {
        retry(until, "узел готов").await;
    }

    // Слушатель подключается в фоне; фиксации с NOTIFY — публикации узла (членство, журнал событий).
    let until = deadline();
    let text = loop {
        let text = running.metrics.encode().expect("metrics");
        if text.contains("tp_db_listener_connected{listener=\"signals\"} 1") {
            break text;
        }
        retry(until, "слушатель LISTEN подключён").await;
    };
    for name in [
        // Пул узла.
        "tp_db_pool_in_use{pool=\"primary\"}",
        "tp_db_pool_idle{pool=\"primary\"}",
        "tp_db_pool_max{pool=\"primary\"}",
        "tp_db_pool_waiting{pool=\"primary\"}",
        "tp_db_pool_wait_seconds_bucket{le=\"+Inf\",pool=\"primary\"}",
        "tp_db_pool_acquire_errors_total{pool=\"primary\"}",
        // Слушатель.
        "tp_db_listener_connected{listener=\"signals\"}",
        // NOTIFY: время фиксаций и число слушателей.
        "tp_db_notify_commit_seconds_bucket{le=\"+Inf\"}",
        "tp_db_notify_commit_seconds_count",
        "tp_db_commit_seconds_bucket{le=\"+Inf\"}",
        "tp_db_listeners 1",
        "tp_db_notify_commit_share",
        // «Сироты».
        "tp_blob_orphans_passes_total",
        "tp_blob_orphans_found_total",
        "tp_blob_orphans_deleted_total",
        "tp_blob_orphans_remaining",
    ] {
        assert!(text.contains(name), "{name}: {text}");
    }
    // Ожидание соединения пулов учтено наблюдателем пула (META-55): узел уже получал соединения.
    for pool in ["primary", "listener"] {
        let series = format!("tp_db_pool_wait_seconds_count{{pool=\"{pool}\"}} ");
        let waits: f64 = text
            .lines()
            .find_map(|l| l.strip_prefix(series.as_str()))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("{series}: {text}"));
        assert!(waits >= 1.0, "{series}: {text}");
    }

    shutdown.cancel();
    let report = running.wait().await.expect("report");
    assert!(report.failed.is_empty(), "{report:?}");
    drop_database(&url, &db_name).await;
}

/// ADM-33: домены организации в Admin API узла — один ресурс `transport/domains` (`TransportStore` `PgMeta`). Список — 200
/// с доменом `admin init`; создание, политика домена и удаление нового домена; удаление домена с учётной записью — отказ;
/// прежнего ресурса `/orgs/{org}/domains` нет (404).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_domains() {
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

    let domain = "example.ru".to_owned();
    let socket = dir.path().join("admin.sock");
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, admin_config(dir.path(), &db_url, &domain, &socket)).expect("config");
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
    let until = deadline();
    while !running.health.is_ready() {
        retry(until, "узел готов").await;
    }
    let admin = addr(&running, "admin");

    // Администратор: учётная запись в принятом домене `example.ru` — от `admin init`.
    let login = format!("admin@{domain}");
    let granted = admin_init(&socket, &login).await;
    drop(bound);
    assert_eq!(granted["status"], "granted", "{granted}");
    let token = granted["token"].as_str().expect("token").to_owned();
    let (code, body) = http(
        admin,
        "POST",
        "/api/v1/session",
        &format!("Bearer {token}"),
        None,
    )
    .await;
    assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
    let session: Value = serde_json::from_slice(&body).expect("session");
    let bearer = format!("Bearer {}", session["session"].as_str().expect("session"));
    let base = format!("/api/v1/orgs/{}/transport/domains", TenantId::DEFAULT);
    let list = || domain_list(admin, &base, &bearer);

    // Список: принятый домен `admin init`.
    let (version, domains) = list().await;
    let own = domains
        .iter()
        .find(|d| d["name"] == domain)
        .unwrap_or_else(|| panic!("{domain} в {domains:?}"))["id"]
        .as_str()
        .expect("id")
        .to_owned();

    // Создание: имя приводится к канонической форме (`normalize_domain`); повтор — 409.
    let (code, body) = http(
        admin,
        "POST",
        &base,
        &bearer,
        Some(&json!({ "version": version, "name": "Corp.Example" })),
    )
    .await;
    assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
    let created: Value = serde_json::from_slice(&body).expect("created");
    assert_eq!(created["domain"]["name"], "corp.example", "{created}");
    assert_eq!(created["domain"]["kind"], "authoritative", "{created}");
    let id = created["domain"]["id"].as_str().expect("id").to_owned();
    let (version, domains) = list().await;
    assert!(domains.iter().any(|d| d["id"] == id));
    let (code, body) = http(
        admin,
        "POST",
        &base,
        &bearer,
        Some(&json!({ "version": version, "name": "corp.example" })),
    )
    .await;
    assert_eq!(code, 409, "{}", String::from_utf8_lossy(&body));

    // Политика домена — на принятом домене: запись и чтение уровня «домен».
    let policy = format!("{base}/{id}/policy");
    let (code, body) = http(
        admin,
        "PUT",
        &policy,
        &bearer,
        Some(&json!({ "max_recipients": 50 })),
    )
    .await;
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    let (code, body) = http(admin, "GET", &policy, &bearer, None).await;
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    let read: Value = serde_json::from_slice(&body).expect("policy");
    assert_eq!(read["policy"]["max_recipients"], 50, "{read}");
    assert_eq!(read["effective"]["max_recipients"], 50, "{read}");

    // Прежний ресурс доменов организации удалён.
    let old = format!("/api/v1/orgs/{}/domains", TenantId::DEFAULT);
    let (code, _) = http(admin, "GET", &old, &bearer, None).await;
    assert_eq!(code, 404);
    let (code, _) = http(admin, "GET", &format!("{old}/{id}"), &bearer, None).await;
    assert_eq!(code, 404);

    // Удаление домена без учётных записей — 200, повтор — 404.
    let item = |version: u64, id: &str| format!("{base}/{id}?version={version}");
    let (code, body) = http(admin, "DELETE", &item(version, &id), &bearer, None).await;
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    let (version, domains) = list().await;
    assert!(!domains.iter().any(|d| d["id"] == id));
    let (code, body) = http(admin, "DELETE", &item(version, &id), &bearer, None).await;
    assert_eq!(code, 404, "{}", String::from_utf8_lossy(&body));

    // Домен с учётной записью администратора не удаляется (внешний ключ — 400), домен на месте.
    let (code, body) = http(admin, "DELETE", &item(version, &own), &bearer, None).await;
    assert_eq!(code, 400, "{}", String::from_utf8_lossy(&body));
    assert!(list().await.1.iter().any(|d| d["id"] == own));

    shutdown.cancel();
    let report = running.wait().await.expect("wait");
    assert!(report.failed.is_empty(), "{report:?}");
    drop_database(&url, &db_name).await;
}

/// `GET /api/v1/orgs/{org}/transport/domains`: 200, версия снимка кластера и принятые домены организации.
async fn domain_list(addr: std::net::SocketAddr, path: &str, bearer: &str) -> (u64, Vec<Value>) {
    let (code, body) = http(addr, "GET", path, bearer, None).await;
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    let list: Value = serde_json::from_slice(&body).expect("domains");
    let version = list["version"].as_u64().expect("version");
    (version, list["domains"].as_array().expect("array").clone())
}
