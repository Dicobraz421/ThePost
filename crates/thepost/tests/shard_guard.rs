//! Защита от потери данных до шардированного GC BLOB (FIX-05): со вторым шардом в справочнике узел с ролью
//! maintenance не стартует, `thepost doctor` — `FAIL` (`database.shards`); с одним шардом — как раньше.
//!
//! Без `TEST_DATABASE_URL` тест пропускается.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use thepost::doctor::{self, Status};
use thepost::doctor_dns::Dns;
use thepost::serve::{ServeOptions, start};
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::{SecretRef, SecretStore};
use tp_testkit::MemSecretStore;

/// DNS без записей: проверкам шардов DNS не нужен, сеть теста не трогаем.
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

/// Адрес без пароля и пароль, если он был в `user:password@` (без раскрытия `%XX`: в CI пароль простой).
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

/// Отдельная база: второй шард в общей базе остановил бы роль maintenance узлов других тестов.
async fn create_database(url: &str) -> (String, String) {
    let name = format!("fix05_{}", uuid_like());
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

fn uuid_like() -> String {
    tp_contracts::ids::DomainId::new()
        .as_uuid()
        .simple()
        .to_string()
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

fn opts(config: &Path, secrets: &Arc<MemSecretStore>) -> (ServeOptions, CancellationToken) {
    let shutdown = CancellationToken::new();
    (
        ServeOptions {
            config: config.to_path_buf(),
            secrets: Some(secrets.clone() as Arc<dyn SecretStore>),
            telemetry: false,
            shutdown: shutdown.clone(),
            modules: thepost::modules::BUILTIN,
        },
        shutdown,
    )
}

async fn doctor_shards(
    config: &Path,
    secrets: &Arc<MemSecretStore>,
) -> (Option<Status>, String, i32) {
    let report = doctor::run_with(
        config,
        Some(secrets.clone() as Arc<dyn SecretStore>),
        Some(&NoDns),
    )
    .await;
    let check = report.get("database.shards").cloned();
    (
        check.as_ref().map(|c| c.status),
        check.map(|c| c.message).unwrap_or_default(),
        report.exit_code(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_shard_blocks_maintenance_and_fails_doctor() {
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
    let blob = dir.path().join("blob");
    let config: PathBuf = dir.path().join("config.yaml");
    std::fs::write(
        &config,
        format!(
            "domain: example.org\nprofile: dev\nroles: [maintenance]\n\
             service_http:\n  listen: 127.0.0.1:0\n  allow_plaintext: true\n\
             database:\n  url: \"{db_url}\"\n  password: {{ secret: \"{db}\" }}\n  tls: {{ allow_plaintext: true }}\n\
             meta:\n  events:\n    node_ttl: 3s\n\
             cluster:\n  node_id: fix05-{}\n\
             runtime:\n  drain_deadline: 5s\n\
             blob:\n  url: file://{}\n",
            std::process::id(),
            blob.display().to_string().replace('\\', "/")
        ),
    )
    .expect("config");

    // Один шард — как раньше: роль maintenance стартует (миграции — при запуске), doctor — OK.
    let (o, shutdown) = opts(&config, &secrets);
    let running = start(o).await.expect("start with one shard");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !running.health.is_ready() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "узел не готов: {:?}",
            running.health.roles_status()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shutdown.cancel();
    let report = running.wait().await.expect("wait");
    assert!(report.failed.is_empty(), "{report:?}");
    let (status, message, _) = doctor_shards(&config, &secrets).await;
    assert_eq!(status, Some(Status::Ok), "{message}");

    // Второй шард в справочнике: GC BLOB его не учитывает — узел не стартует, doctor — FAIL с причиной.
    let pool = sqlx::PgPool::connect(&url_db).await.expect("pool");
    sqlx::query(
        "INSERT INTO shards (shard_id, name, dsn, accepts_new) \
         VALUES (1, 'second', 'postgres://thepost@127.0.0.1:1/second', false)",
    )
    .execute(&pool)
    .await
    .expect("second shard");
    pool.close().await;

    let (o, _) = opts(&config, &secrets);
    let e = match start(o).await {
        Ok(_) => panic!("узел с ролью maintenance запущен при двух шардах"),
        Err(e) => e.to_string(),
    };
    assert!(
        e.contains("FIX-04") && e.contains("шардов в справочнике: 2"),
        "{e}"
    );
    let (status, message, code) = doctor_shards(&config, &secrets).await;
    assert_eq!(status, Some(Status::Fail), "{message}");
    assert!(message.contains("FIX-04"), "{message}");
    assert_eq!(code, 1);

    drop_database(&admin_url, &db_name).await;
}
