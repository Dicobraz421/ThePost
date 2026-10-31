//! Общее для интеграционных тестов на PostgreSQL: без `TEST_DATABASE_URL` тесты пропускаются.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tp_contracts::secrets::{Secret, SecretRef, SecretStore};
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

/// Секция пула для интеграционных тестов: профиль dev с `tls.allow_plaintext` (META-14); пароль из адреса
/// тестовой БД перенесён в хранилище секретов [`secrets`] под ссылкой [`DB_SECRET`] (META-17).
pub fn dev_pool_config(url: &str) -> PoolConfig {
    let (url, password) = split_password(url);
    PoolConfig {
        url: dev_url(&url),
        password: password.map(|p| {
            let r = db_secret();
            secrets().set(&r, &p);
            r
        }),
        tls: InternalTls {
            allow_plaintext: true,
            ..InternalTls::default()
        },
        profile: Profile::Dev,
        ..PoolConfig::default()
    }
}

/// Ссылка на пароль тестовой БД в [`secrets`].
pub const DB_SECRET: &str = "test/db#password";

/// [`DB_SECRET`] как ссылка.
pub fn db_secret() -> SecretRef {
    DB_SECRET.parse().expect("ссылка")
}

/// Адрес без пароля и пароль (с раскрытыми `%XX`), если он был в `user:password@`.
pub fn split_password(url: &str) -> (String, Option<String>) {
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
        Some(percent_decode(password)),
    )
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).expect("utf-8")
}

/// Хранилище секретов тестов (до `MemSecretStore` из tp-testkit, KIT-06): значения из кода теста, отзыв,
/// отказ хранилища целиком.
#[derive(Debug, Default)]
pub struct MemSecrets {
    values: Mutex<HashMap<SecretRef, String>>,
    unavailable: AtomicBool,
}

impl MemSecrets {
    /// Задаёт значение секрета.
    pub fn set(&self, r: &SecretRef, value: &str) {
        self.values
            .lock()
            .expect("lock")
            .insert(r.clone(), value.to_owned());
    }

    /// Отзывает секрет: дальше — `NotFound`.
    pub fn revoke(&self, r: &SecretRef) {
        self.values.lock().expect("lock").remove(r);
    }

    /// Хранилище недоступно (запечатано): все чтения — `Unavailable`.
    pub fn set_unavailable(&self, on: bool) {
        self.unavailable.store(on, Ordering::SeqCst);
    }
}

#[async_trait]
impl SecretStore for MemSecrets {
    async fn get(&self, r: &SecretRef) -> tp_contracts::Result<Secret> {
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(tp_contracts::Error::UNAVAILABLE);
        }
        self.values
            .lock()
            .expect("lock")
            .get(r)
            .map(Secret::new)
            .ok_or(tp_contracts::Error::NotFound)
    }
}

/// Общее хранилище секретов тестов процесса.
pub fn secrets() -> Arc<MemSecrets> {
    static STORE: OnceLock<Arc<MemSecrets>> = OnceLock::new();
    STORE.get_or_init(Arc::default).clone()
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

/// Отдельная чистая БД для тестов системных выборок по всем организациям (очередь, истёкшая корзина,
/// перестройки индексов): записи других тестов и прежних запусков в них не попадают.
pub struct FreshDb {
    /// Хранилища над чистой БД.
    pub meta: PgMeta,
    admin: PgPool,
    name: String,
}

impl FreshDb {
    /// Удаляет БД (в конце теста).
    pub async fn drop_db(self) {
        use sqlx::{AssertSqlSafe, Executor};
        self.meta.pool().close().await;
        self.admin
            .execute(AssertSqlSafe(format!(
                "DROP DATABASE {} WITH (FORCE)",
                self.name
            )))
            .await
            .expect("drop database");
    }
}

/// Хранилища над новой чистой БД со схемой.
pub async fn fresh_meta() -> Option<FreshDb> {
    use std::str::FromStr;

    use sqlx::postgres::PgConnectOptions;
    use sqlx::{AssertSqlSafe, Executor};
    let url = pg_url()?;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("подключение к TEST_DATABASE_URL");
    let name = format!("tp_meta_{}", uuid::Uuid::now_v7().simple());
    admin
        .execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .await
        .expect("create database");
    let opts = PgConnectOptions::from_str(&url)
        .expect("url")
        .database(&name);
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect_with(opts)
        .await
        .expect("подключение к чистой БД");
    let meta = PgMeta::open(pool, MetaConfig::default())
        .await
        .expect("open");
    Some(FreshDb { meta, admin, name })
}
