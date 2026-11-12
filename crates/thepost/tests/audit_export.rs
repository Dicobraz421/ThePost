//! Выгрузка журнала аудита в SIEM из базы (APP-59; решение владельца 12.11: база + выгрузка из неё): узел с ролями
//! admin и maintenance пишет записи аудита (`admin init` по локальному сокету) только в журнал, роль maintenance
//! выгружает их в тестовый приёмник syslog (RFC 5424 кадрами RFC 5425, открытый канал профиля dev).
//!
//! Приёмник недоступен, пока появляются 10 записей; после его возврата приходят все 10 по порядку номеров, без
//! повторов. Новый адрес приёмника в перечитанном config.yaml действует без перезапуска. `thepost doctor` —
//! `admin.audit.syslog`: `FAIL`, если приёмник не принимает соединения, `OK` — если принимает.
//!
//! Без `TEST_DATABASE_URL` тест пропускается.

#![cfg(unix)]

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use thepost::doctor::{self, Status};
use thepost::doctor_dns::Dns;
use thepost::serve::{ServeOptions, start};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::{SecretRef, SecretStore};
use tp_testkit::MemSecretStore;

/// Маршрут записи аудита `admin init` (`tp_admin::init`).
const INIT_ROUTE: &str = "local:admin init";

/// Предел ожидания шага.
const WAIT: Duration = Duration::from_secs(30);

/// Записей, пока приёмник недоступен.
const OUTAGE_RECORDS: usize = 10;

/// DNS без записей: проверкам выгрузки DNS не нужен, сеть теста не трогаем.
struct NoDns;

#[async_trait]
impl Dns for NoDns {
    async fn mx(&self, _: &str) -> Result<Vec<(u16, String)>, String> {
        Ok(Vec::new())
    }
    async fn txt(&self, _: &str) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
    async fn ip(&self, _: &str) -> Result<Vec<IpAddr>, String> {
        Ok(Vec::new())
    }
    async fn ptr(&self, _: IpAddr) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
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
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return (url.to_owned(), None);
    };
    let Some((user, password)) = userinfo.split_once(':') else {
        return (url.to_owned(), None);
    };
    (
        format!("{scheme}://{user}@{host}{tail}"),
        Some(password.to_owned()),
    )
}

/// Отдельная база теста: журнал аудита и отметки выгрузки — только узла теста.
async fn create_database(url: &str) -> (String, String) {
    let name = format!(
        "app59_{}",
        tp_contracts::ids::DomainId::new().as_uuid().simple()
    );
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

/// config.yaml узла с ролями admin и maintenance: Admin API, локальный сокет `admin init`, выгрузка аудита в syslog
/// `syslog` пачками по 4 записи.
fn config(dir: &Path, db_url: &str, socket: &Path, syslog: &str) -> String {
    let path = |p: &Path| p.display().to_string().replace('\\', "/");
    format!(
        "domain: example.org\nprofile: dev\nroles: [admin, maintenance]\n\
         service_http:\n  listen: 127.0.0.1:0\n  allow_plaintext: true\n\
         database:\n  url: \"{db_url}\"\n  password: {{ secret: \"thepost/db#password\" }}\n  tls: {{ allow_plaintext: true }}\n\
         meta:\n  events:\n    node_ttl: 3s\n\
         cluster:\n  node_id: app59-{pid}\n\
         runtime:\n  drain_deadline: 5s\n\
         blob:\n  url: file://{blob}\n\
         http:\n  admin: {{ listen: \"127.0.0.1:0\", allow_plaintext: true }}\n\
         admin:\n  init:\n    socket: \"{socket}\"\n  audit:\n    interval: 100ms\n    retry: 200ms\n    batch: 4\n    \
         syslog:\n      address: \"{syslog}\"\n      connect_timeout: 1s\n      tls: {{ allow_plaintext: true }}\n",
        pid = std::process::id(),
        blob = path(&dir.join("blob")),
        socket = path(socket),
    )
}

/// `thepost admin init` по локальному сокету узла: ответ узла.
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

/// Приёмник syslog: соединение и непрочитанный остаток кадров.
struct Receiver {
    sock: TcpStream,
    buf: Vec<u8>,
}

impl Receiver {
    /// Первое соединение узла с `listener`.
    async fn accept(listener: &TcpListener) -> Self {
        let (sock, _) = tokio::time::timeout(WAIT, listener.accept())
            .await
            .expect("узел подключился к приёмнику syslog")
            .expect("accept");
        Self {
            sock,
            buf: Vec::new(),
        }
    }

    /// Следующий целый кадр из буфера: запись JSON после BOM.
    fn next_frame(&mut self) -> Option<Value> {
        let sp = self.buf.iter().position(|b| *b == b' ')?;
        let len: usize = std::str::from_utf8(&self.buf[..sp])
            .expect("длина кадра")
            .parse()
            .expect("длина кадра");
        let msg = self.buf.get(sp + 1..sp + 1 + len)?;
        let msg = std::str::from_utf8(msg).expect("utf-8");
        // facility 13 (log audit), severity 5 (notice): PRI 109.
        assert!(msg.starts_with("<109>1 "), "{msg}");
        let json = msg.split_once('\u{feff}').expect("BOM").1;
        let value = serde_json::from_str(json).expect("json");
        self.buf.drain(..sp + 1 + len);
        Some(value)
    }

    /// `n` записей.
    async fn records(&mut self, n: usize) -> Vec<Value> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + WAIT;
        let mut chunk = [0u8; 4096];
        loop {
            while let Some(v) = self.next_frame() {
                out.push(v);
            }
            if out.len() >= n {
                return out;
            }
            let read = tokio::time::timeout_at(deadline, self.sock.read(&mut chunk))
                .await
                .expect("записи syslog вовремя")
                .expect("read");
            assert!(read > 0, "соединение закрыто: получено {}", out.len());
            self.buf.extend_from_slice(&chunk[..read]);
        }
    }

    /// За `quiet` больше ничего не пришло.
    async fn silent(&mut self, quiet: Duration) {
        let mut chunk = [0u8; 4096];
        assert!(self.buf.is_empty(), "лишние байты в приёмнике");
        if let Ok(read) = tokio::time::timeout(quiet, self.sock.read(&mut chunk)).await {
            let read = read.expect("read");
            assert_eq!(read, 0, "повтор или лишняя запись: {:?}", &chunk[..read]);
        }
    }
}

/// Свободный адрес без слушателя: подключение к нему отклоняется.
async fn unused_addr() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    l.local_addr().expect("addr")
}

/// Проверка `admin.audit.syslog` отчёта `thepost doctor` по config.yaml `path`.
async fn doctor_syslog(path: &Path, secrets: &Arc<MemSecretStore>) -> (Option<Status>, String) {
    let report = doctor::run_with(
        path,
        Some(secrets.clone() as Arc<dyn SecretStore>),
        Some(&NoDns),
    )
    .await;
    let check = report.get("admin.audit.syslog").cloned();
    (
        check.as_ref().map(|c| c.status),
        check.map(|c| c.message).unwrap_or_default(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_records_wait_in_journal_until_syslog_returns() {
    let Some(url) = tp_testkit::pg_url() else {
        eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
        return;
    };
    let admin_url = dev_url(&url);
    let (db_name, url_db) = create_database(&admin_url).await;
    let (db_url, password) = split_password(&url_db);
    let secrets = Arc::new(MemSecretStore::default());
    let db: SecretRef = "thepost/db#password".parse().expect("ref");
    secrets.set(&db, password.unwrap_or_else(|| "unused".to_owned()));
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("admin.sock");

    // doctor: приёмник не принимает соединения — FAIL с адресом; принимает — OK.
    let closed = unused_addr().await.to_string();
    let probe = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let doctor_cfg: PathBuf = dir.path().join("doctor.yaml");
    std::fs::write(&doctor_cfg, config(dir.path(), &db_url, &socket, &closed)).expect("config");
    let (status, message) = doctor_syslog(&doctor_cfg, &secrets).await;
    assert_eq!(status, Some(Status::Fail), "{message}");
    assert!(message.contains(&closed), "{message}");
    let open = probe.local_addr().expect("addr").to_string();
    std::fs::write(&doctor_cfg, config(dir.path(), &db_url, &socket, &open)).expect("config");
    let (status, message) = doctor_syslog(&doctor_cfg, &secrets).await;
    assert_eq!(status, Some(Status::Ok), "{message}");

    // Узел: приёмник по адресу из config.yaml пока недоступен.
    let first = unused_addr().await;
    let path: PathBuf = dir.path().join("config.yaml");
    std::fs::write(
        &path,
        config(dir.path(), &db_url, &socket, &first.to_string()),
    )
    .expect("config");
    let cfg = thepost::sections::registry()
        .expect("registry")
        .load(&path)
        .expect("load config");
    thepost::local_admin::bind(&cfg).expect("admin init socket");
    let shutdown = CancellationToken::new();
    let running = start(ServeOptions {
        config: path.clone(),
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
            "узел не готов: {:?}",
            running.health.roles_status()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 10 записей при недоступном приёмнике: первый `admin init` и 9 отказов-повторов (409).
    let login = "admin@example.org";
    let granted = admin_init(&socket, login).await;
    assert_eq!(granted["status"], "granted", "{granted}");
    for _ in 1..OUTAGE_RECORDS {
        let refused = admin_init(&socket, login).await;
        assert_eq!(refused["status"], "refused", "{refused}");
    }
    // Выгрузка за это время пыталась доставить (retry 200ms) и не смогла.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Приёмник вернулся: все 10 записей по порядку номеров, без повторов.
    let listener = TcpListener::bind(first)
        .await
        .expect("приёмник на прежнем адресе");
    let mut rx = Receiver::accept(&listener).await;
    let records = rx.records(OUTAGE_RECORDS).await;
    assert_eq!(records.len(), OUTAGE_RECORDS, "{records:?}");
    let seqs: Vec<u64> = records
        .iter()
        .map(|r| r["seq"].as_u64().expect("seq"))
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "порядок: {seqs:?}");
    for (i, r) in records.iter().enumerate() {
        assert_eq!(r["route"], INIT_ROUTE, "{r}");
        assert_eq!(r["method"], "LOCAL", "{r}");
        assert_eq!(r["actor_login"], login, "{r}");
        assert_eq!(r["status"], if i == 0 { 201 } else { 409 }, "{r}");
    }
    rx.silent(Duration::from_millis(1500)).await;
    let last = seqs[OUTAGE_RECORDS - 1];

    // Новый адрес приёмника — без перезапуска: следующая запись — во второй приёмник, с продолжением номеров.
    let second = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let second_addr = second.local_addr().expect("addr").to_string();
    std::fs::write(&path, config(dir.path(), &db_url, &socket, &second_addr)).expect("config");
    let until = tokio::time::Instant::now() + WAIT;
    let mut rx2 = loop {
        assert!(
            tokio::time::Instant::now() < until,
            "новая секция admin.audit не применена"
        );
        let refused = admin_init(&socket, login).await;
        assert_eq!(refused["status"], "refused", "{refused}");
        if let Ok(accepted) =
            tokio::time::timeout(Duration::from_millis(500), second.accept()).await
        {
            break Receiver {
                sock: accepted.expect("accept").0,
                buf: Vec::new(),
            };
        }
    };
    let next = rx2.records(1).await;
    assert!(
        next[0]["seq"].as_u64().is_some_and(|s| s > last),
        "{next:?}"
    );
    assert_eq!(next[0]["status"], 409, "{next:?}");

    shutdown.cancel();
    let report = running.wait().await.expect("wait");
    assert!(report.failed.is_empty(), "{report:?}");
    drop_database(&admin_url, &db_name).await;
}
