//! APP-63: роль maintenance при запуске пересчитывает поисковые строки контактов, записанных до миграции 0030
//! (META-78): старая карточка после запуска узла находится поиском.
//!
//! Без `TEST_DATABASE_URL` тест пропускается.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use thepost::serve::{ServeOptions, start};
use tp_contracts::dav::{
    Collection, CollectionKind, Component, DavStore, ObjectIndex, Precondition, PutOutcome,
};
use tp_contracts::meta::{AccountKind, AccountScope, AccountStore, AccountUpsert};
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::{SecretRef, SecretStore};
use tp_contracts::{DomainId, TenantId};
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
    DomainId::new().as_uuid().simple().to_string()
}

/// Отдельная база: ответ — имя и адрес.
async fn create_database(url: &str) -> (String, String) {
    let name = format!("app63_{}", random());
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

/// Учётная запись организации по умолчанию в новом домене.
async fn account(meta: &PgMeta, pool: &DbPool) -> AccountScope {
    let tenant = TenantId::DEFAULT;
    let domain = DomainId::new();
    let name = format!("d{}.test", random());
    let mut tx = tp_meta::schema::begin(pool, tenant).await.expect("begin");
    sqlx::query("INSERT INTO domains (tenant_id, domain_id, name) VALUES ($1, $2, $3)")
        .bind(tenant.as_uuid())
        .bind(domain.as_uuid())
        .bind(&name)
        .execute(&mut *tx)
        .await
        .expect("domain");
    tx.commit().await.expect("commit");
    let acc = meta
        .upsert(
            tenant,
            AccountUpsert {
                domain,
                login: format!("u{}@{name}", random()),
                display_name: "User".into(),
                external_id: None,
                enabled: true,
                aliases: Vec::new(),
                kind: AccountKind::User,
            },
        )
        .await
        .expect("upsert");
    AccountScope {
        tenant,
        account: acc.id,
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

async fn found(meta: &PgMeta, scope: AccountScope, book: &Collection) -> Vec<String> {
    meta.find_cards(scope, book.id, "петров", 10)
        .await
        .expect("find")
        .into_iter()
        .map(|o| o.name)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maintenance_reindexes_old_cards_on_start() {
    let Some(url) = tp_testkit::pg_url() else {
        eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
        return;
    };
    let admin_url = dev_url(&url);
    let (db_name, url_db) = create_database(&admin_url).await;
    let pool = DbPool::new(
        sqlx::PgPool::connect(&url_db).await.expect("pool"),
        Arc::new(NoAcquireObserver),
    );
    tp_meta::schema::migrate(&pool).await.expect("migrate");
    let meta = PgMeta::new(pool.clone(), MetaConfig::default());
    let scope = account(&meta, &pool).await;
    let book = Collection::new(CollectionKind::AddressBook, "contacts");
    meta.create_collection(scope, book.clone())
        .await
        .expect("create");
    // Карточка до миграции 0030: без поисковых строк, признак «не посчитаны».
    let body = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:old\r\nFN:Пётр Петров\r\nN:Петров;Пётр;;;\r\n\
                EMAIL:petrov@example.org\r\nEND:VCARD\r\n";
    match meta
        .put_object(
            scope,
            book.id,
            "old.vcf",
            body.to_owned(),
            ObjectIndex::new(Component::Card, "old", None),
            Precondition::Any,
        )
        .await
        .expect("put")
    {
        PutOutcome::Stored(_) => {}
        other => panic!("карточка не записана: {other:?}"),
    }
    let mut tx = tp_meta::schema::begin_system(&pool).await.expect("begin");
    sqlx::query("UPDATE dav_objects SET search_pending = true WHERE component = 'VCARD'")
        .execute(&mut *tx)
        .await
        .expect("pending");
    tx.commit().await.expect("commit");
    assert!(
        found(&meta, scope, &book).await.is_empty(),
        "до пересчёта не находится"
    );

    let (db_url, password) = split_password(&url_db);
    let secrets = Arc::new(MemSecretStore::default());
    let db: SecretRef = "thepost/db#password".parse().expect("ref");
    secrets.set(&db, password.unwrap_or_else(|| "unused".to_owned()));
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("config.yaml");
    std::fs::write(
        &config,
        format!(
            "domain: example.org\nprofile: dev\nroles: [maintenance]\n\
             service_http:\n  listen: 127.0.0.1:0\n  allow_plaintext: true\n\
             database:\n  url: \"{db_url}\"\n  password: {{ secret: \"{db}\" }}\n  tls: {{ allow_plaintext: true }}\n\
             meta:\n  events:\n    node_ttl: 3s\n\
             cluster:\n  node_id: app63-{}\n\
             runtime:\n  drain_deadline: 5s\n\
             blob:\n  url: file://{}\n",
            std::process::id(),
            dir.path()
                .join("blob")
                .display()
                .to_string()
                .replace('\\', "/")
        ),
    )
    .expect("config");

    let (o, shutdown) = opts(&config, &secrets);
    let running = start(o).await.expect("start");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while found(&meta, scope, &book).await != ["old.vcf"] {
        assert!(
            tokio::time::Instant::now() < deadline,
            "старая карточка не пересчитана: роли {:?}",
            running.health.roles_status()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    shutdown.cancel();
    let report = running.wait().await.expect("wait");
    assert!(report.failed.is_empty(), "{report:?}");

    pool.close().await;
    drop_database(&admin_url, &db_name).await;
}
