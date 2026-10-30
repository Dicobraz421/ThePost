//! Общее для интеграционных тестов на PostgreSQL: без `TEST_DATABASE_URL` тесты пропускаются.

#![allow(dead_code)]

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tp_contracts::tls::{InternalTls, Profile};
use tp_contracts::{DomainId, TenantId};
use tp_meta::pool::PoolConfig;
use tp_meta::{MetaConfig, PgMeta, schema};

/// Адрес тестовой БД.
pub fn pg_url() -> Option<String> {
    let url = std::env::var("TEST_DATABASE_URL").ok();
    if url.is_none() {
        eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
    }
    url
}

/// Адрес тестовой БД для пулов из конфигурации: без `sslmode` — открытый канал (`sslmode=disable`).
pub fn dev_url(url: &str) -> String {
    if url.contains("sslmode=") {
        return url.to_owned();
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}sslmode=disable")
}

/// Секция пула для интеграционных тестов: профиль dev с `tls.allow_plaintext` (META-14).
pub fn dev_pool_config(url: &str) -> PoolConfig {
    PoolConfig {
        url: dev_url(url),
        tls: InternalTls {
            allow_plaintext: true,
            ..InternalTls::default()
        },
        profile: Profile::Dev,
        ..PoolConfig::default()
    }
}

/// Пул к тестовой БД с применённой схемой.
pub async fn pool() -> Option<PgPool> {
    let url = pg_url()?;
    let pool = PgPoolOptions::new()
        .max_connections(48)
        .connect(&url)
        .await
        .expect("подключение к TEST_DATABASE_URL");
    schema::migrate(&pool).await.expect("миграции");
    Some(pool)
}

/// Хранилища над тестовой БД.
pub async fn meta() -> Option<PgMeta> {
    let pool = pool().await?;
    Some(
        PgMeta::open(pool, MetaConfig::default())
            .await
            .expect("open"),
    )
}

/// Новая организация (тесты изолированы друг от друга своими организациями).
pub async fn tenant(pool: &PgPool) -> TenantId {
    let tenant = TenantId::new();
    let mut tx = schema::begin(pool, tenant).await.expect("begin");
    sqlx::query("INSERT INTO tenants (tenant_id, name) VALUES ($1, 'test')")
        .bind(tenant.as_uuid())
        .execute(&mut *tx)
        .await
        .expect("tenant");
    tx.commit().await.expect("commit");
    tenant
}

/// Новый домен организации со случайным именем; возвращает идентификатор и имя.
pub async fn domain(pool: &PgPool, tenant: TenantId) -> (DomainId, String) {
    let id = DomainId::new();
    let name = format!("d{}.test", id.as_uuid().simple());
    let mut tx = schema::begin(pool, tenant).await.expect("begin");
    sqlx::query("INSERT INTO domains (tenant_id, domain_id, name) VALUES ($1, $2, $3)")
        .bind(tenant.as_uuid())
        .bind(id.as_uuid())
        .bind(&name)
        .execute(&mut *tx)
        .await
        .expect("domain");
    tx.commit().await.expect("commit");
    (id, name)
}

/// Новая учётная запись в новой организации.
pub async fn account(meta: &PgMeta) -> tp_contracts::meta::AccountScope {
    use tp_contracts::meta::{AccountStore, AccountUpsert};
    let tenant = tenant(meta.pool()).await;
    let (domain, name) = domain(meta.pool(), tenant).await;
    let acc = meta
        .upsert(
            tenant,
            AccountUpsert {
                domain,
                login: format!("user@{name}"),
                display_name: "User".into(),
                external_id: None,
                enabled: true,
            },
        )
        .await
        .expect("upsert");
    tp_contracts::meta::AccountScope {
        tenant,
        account: acc.id,
    }
}
