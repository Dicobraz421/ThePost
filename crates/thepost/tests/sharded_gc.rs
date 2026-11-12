//! FIX-04: GC и поиск «сирот» BLOB роли maintenance — по ссылкам всех шардов справочника. Два шарда — две базы
//! одного PostgreSQL: объект, на который ссылается только второй шард, не удаляется; объект без ссылок и объект
//! без записи учёта — удаляются.
//!
//! Без `TEST_DATABASE_URL` тест пропускается.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use thepost::serve::{ServeOptions, start};
use tp_blob::{LocalConfig, LocalStore};
use tp_contracts::blob::{BlobIndex, BlobStore, Commit};
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::{SecretRef, SecretStore};
use tp_contracts::{BlobHash, BlobKey, Gen, TenantId};
use tp_meta::pool::{DbPool, NoAcquireObserver};
use tp_meta::{MetaConfig, PgMeta};
use tp_testkit::MemSecretStore;

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

fn random() -> String {
    tp_contracts::ids::DomainId::new()
        .as_uuid()
        .simple()
        .to_string()
}

/// Новая база `prefix_<случайное>`; ответ — имя и адрес.
async fn create_database(url: &str, prefix: &str) -> (String, String) {
    let name = format!("{prefix}_{}", random());
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

/// Пул к базе `url` со схемой ядра.
async fn migrated(url: &str) -> DbPool {
    let pool = DbPool::new(
        sqlx::PgPool::connect(url).await.expect("pool"),
        Arc::new(NoAcquireObserver),
    );
    tp_meta::schema::migrate(&pool).await.expect("migrate");
    pool
}

fn key(tenant: TenantId) -> BlobKey {
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(tp_contracts::ids::DomainId::new().as_uuid().as_bytes());
    h[16..].copy_from_slice(tp_contracts::ids::DomainId::new().as_uuid().as_bytes());
    BlobKey {
        tenant,
        hash: BlobHash(h),
        generation: Gen::INITIAL,
    }
}

/// Запись учёта основной БД без ссылок (загрузка и снятие ссылки): кандидат GC по истечении окна W.
async fn unreferenced(main: &PgMeta, key: &BlobKey) {
    main.acquire(key.tenant, None, &key.hash, 1)
        .await
        .expect("acquire");
    assert!(matches!(
        main.commit_new(key.tenant, &key.hash, key.generation, 2, 1)
            .await,
        Ok(Commit::Done { .. })
    ));
    main.release(key.tenant, &key.hash, key.generation, 1)
        .await
        .expect("release");
}

/// Ссылка на объект на шарде `shard` (копия письма в ящике этого шарда).
async fn referenced(shard: &PgMeta, key: &BlobKey) {
    assert!(matches!(
        shard
            .commit_new(key.tenant, &key.hash, key.generation, 2, 1)
            .await,
        Ok(Commit::Done { .. })
    ));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_keeps_object_referenced_only_on_second_shard() {
    let Some(url) = tp_testkit::pg_url() else {
        eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
        return;
    };
    let admin_url = dev_url(&url);
    let (main_name, main_url) = create_database(&admin_url, "fix04_main").await;
    let (shard_name, shard_url) = create_database(&admin_url, "fix04_shard").await;
    let main_pool = migrated(&main_url).await;
    let shard_pool = migrated(&shard_url).await;

    let secrets = Arc::new(MemSecretStore::default());
    let (db_url, password) = split_password(&main_url);
    let db: SecretRef = "thepost/db#password".parse().expect("ref");
    secrets.set(&db, password.unwrap_or_else(|| "unused".to_owned()));
    // Второй шард в справочнике основной БД: адрес без пароля, пароль — секрет `credentials`.
    let (shard_dsn, shard_password) = split_password(&shard_url);
    let credentials = shard_password.map(|p| {
        let r: SecretRef = "thepost/shard#password".parse().expect("ref");
        secrets.set(&r, p);
        r.to_string()
    });
    sqlx::query(
        "INSERT INTO shards (shard_id, name, dsn, credentials, accepts_new) VALUES (1, 'second', $1, $2, false)",
    )
    .bind(dev_url(&shard_dsn))
    .bind(credentials)
    .execute(&mut *main_pool.acquire().await.expect("conn"))
    .await
    .expect("second shard");

    let tenant = TenantId::new();
    let mut tx = tp_meta::schema::begin(&main_pool, tenant)
        .await
        .expect("begin");
    sqlx::query("INSERT INTO tenants (tenant_id, name) VALUES ($1, 'fix04')")
        .bind(tenant.as_uuid())
        .execute(&mut *tx)
        .await
        .expect("tenant");
    tx.commit().await.expect("commit");

    let dir = tempfile::tempdir().expect("tempdir");
    let blob_root = dir.path().join("blob");
    let store = LocalStore::open(LocalConfig {
        root: blob_root.clone(),
        fsync: false,
        ..LocalConfig::default()
    })
    .await
    .expect("blob store");
    let main = PgMeta::new(main_pool.clone(), MetaConfig::default());
    let second = PgMeta::new(shard_pool.clone(), MetaConfig::default());
    // shared: в основной БД ссылок нет, на втором шарде — одна; garbage: ссылок нет нигде; only_second: запись
    // учёта только на втором шарде; orphan: записи учёта нет нигде.
    let (shared, garbage, only_second, orphan) =
        (key(tenant), key(tenant), key(tenant), key(tenant));
    unreferenced(&main, &shared).await;
    referenced(&second, &shared).await;
    unreferenced(&main, &garbage).await;
    referenced(&second, &only_second).await;
    for k in [&shared, &garbage, &only_second, &orphan] {
        store.put(k, Bytes::from_static(b"ok")).await.expect("put");
    }

    let config: PathBuf = dir.path().join("config.yaml");
    std::fs::write(
        &config,
        format!(
            "domain: example.org\nprofile: dev\nroles: [maintenance]\n\
             service_http:\n  listen: 127.0.0.1:0\n  allow_plaintext: true\n\
             database:\n  url: \"{db_url}\"\n  password: {{ secret: \"{db}\" }}\n  tls: {{ allow_plaintext: true }}\n\
             meta:\n  events:\n    node_ttl: 3s\n\
             cluster:\n  node_id: fix04-{}\n\
             runtime:\n  drain_deadline: 5s\n\
             blob:\n  url: file://{}\n\
             \x20 gc: {{ window: 1s, max_wait: 1s, retry: 1s }}\n\
             \x20 orphans: {{ grace: 1s, interval: 1s }}\n",
            std::process::id(),
            blob_root.display().to_string().replace('\\', "/")
        ),
    )
    .expect("config");

    let (o, shutdown) = opts(&config, &secrets);
    let running = start(o).await.expect("start with two shards");
    // GC удаляет объект без ссылок, поиск «сирот» — объект без записи учёта; оба прохода видят и остальные объекты.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let garbage_gone = !main.exists(&garbage).await.expect("exists")
            && store.head(&garbage).await.expect("head").is_none();
        let orphan_gone = store.head(&orphan).await.expect("head").is_none();
        if garbage_gone && orphan_gone {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "GC и «сироты» не прошли: garbage удалён — {garbage_gone}, orphan удалён — {orphan_gone}; роли: {:?}",
            running.health.roles_status()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Ещё один проход GC и «сирот» после удаления: ссылки второго шарда держат объекты.
    tokio::time::sleep(Duration::from_secs(3)).await;
    shutdown.cancel();
    let report = running.wait().await.expect("wait");
    assert!(report.failed.is_empty(), "{report:?}");

    assert!(
        store.head(&shared).await.expect("head").is_some(),
        "объект со ссылкой на втором шарде удалён из хранилища"
    );
    assert!(main.exists(&shared).await.expect("exists"));
    assert!(second.exists(&shared).await.expect("exists"));
    assert!(
        store.head(&only_second).await.expect("head").is_some(),
        "объект с записью учёта только на втором шарде удалён как «сирота»"
    );
    assert!(second.exists(&only_second).await.expect("exists"));

    main_pool.close().await;
    shard_pool.close().await;
    drop_database(&admin_url, &main_name).await;
    drop_database(&admin_url, &shard_name).await;
}
