//! APP-67: роль maintenance окончательно удаляет отключённый ящик с истёкшим сроком хранения
//! (`DisabledMailboxes::run` под лизой `disabled-mailboxes`), а `/metrics` узла показывает истечение корзины рядами
//! `tp_maintenance_purge_*{step="expire_trash"}`.
//!
//! Без `TEST_DATABASE_URL` тест пропускается.

use std::sync::Arc;
use std::time::Duration;

use thepost::roles::DISABLED_LEASE;
use thepost::serve::{ServeOptions, start};
use tp_contracts::lease::LeaseStore;
use tp_contracts::meta::{AccountKind, AccountScope, AccountState, AccountStore, AccountUpsert};
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::{SecretRef, SecretStore};
use tp_contracts::{DomainId, Error, TenantId};
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
    let name = format!("app67_{}", random());
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
                hidden_from_gal: false,
            },
        )
        .await
        .expect("upsert");
    AccountScope {
        tenant,
        account: acc.id,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maintenance_removes_expired_disabled_mailbox() {
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
    // Срок хранения отключённого ящика истёк до запуска узла; действующий ящик рядом не трогается.
    let expired = account(&meta, &pool).await;
    let active = account(&meta, &pool).await;
    let until = (tp_blob::system_now())() - Duration::from_secs(60);
    meta.disable_account(expired.tenant, expired.account, until)
        .await
        .expect("disable");

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
             cluster:\n  node_id: app67-{}\n\
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

    let shutdown = CancellationToken::new();
    let running = start(ServeOptions {
        config: config.clone(),
        secrets: Some(secrets.clone() as Arc<dyn SecretStore>),
        telemetry: false,
        shutdown: shutdown.clone(),
        modules: thepost::modules::BUILTIN,
    })
    .await
    .expect("start");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match meta.account_state(expired.tenant, expired.account).await {
            Err(Error::NotFound) => break,
            state => assert!(
                tokio::time::Instant::now() < deadline,
                "отключённый ящик не удалён ({state:?}): роли {:?}",
                running.health.roles_status()
            ),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        meta.account_state(active.tenant, active.account)
            .await
            .expect("active"),
        AccountState::Active
    );
    // Удаление ведёт держатель лизы — этот узел: другой её не получит.
    let other = meta
        .try_acquire(DISABLED_LEASE, "other-node", Duration::from_secs(30))
        .await
        .expect("try_acquire");
    assert!(other.is_none(), "лиза не у узла: {other:?}");

    let text = running.metrics.encode().expect("metrics");
    for name in [
        "tp_maintenance_purge_runs_total{step=\"expire_trash\"}",
        "tp_maintenance_purge_errors_total{step=\"expire_trash\"}",
        "tp_maintenance_purged_total{step=\"expire_trash\"}",
        "tp_maintenance_purge_last_success_timestamp_seconds{step=\"expire_trash\"}",
    ] {
        assert!(text.contains(name), "{name}: {text}");
    }

    shutdown.cancel();
    let report = running.wait().await.expect("wait");
    assert!(report.failed.is_empty(), "{report:?}");
    // После остановки роли лиза отдана.
    let other = meta
        .try_acquire(DISABLED_LEASE, "other-node", Duration::from_secs(30))
        .await
        .expect("try_acquire");
    assert!(other.is_some(), "лиза не отдана после остановки");

    pool.close().await;
    drop_database(&admin_url, &db_name).await;
}
