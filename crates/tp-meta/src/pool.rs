//! Пул соединений PostgreSQL из конфигурации и транзакции в контексте организации (SRS 3.1, 3.8).
//!
//! Режим PgBouncer transaction ([`PoolConfig::pgbouncer`]): соединение сервера меняется между транзакциями,
//! поэтому состояние сеанса не используется — контекст организации задаёт `SET LOCAL` ([`with_tenant`]),
//! кэш подготовленных выражений отключён (безымянные выражения работают с любой версией PgBouncer).
//! Миграции держат рекомендательную блокировку сеанса — их пул подключается в обход PgBouncer
//! ([`PoolConfig::direct_url`], [`connect_direct`]).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgConnection, PgPool};
use tp_contracts::{Error, Result, TenantId};

use crate::schema::{begin, begin_system, db_err};

/// Секция конфигурации пула соединений `database` (SRS 5.5). Все параметры требуют перезапуска.
#[derive(Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// Адрес PostgreSQL или PgBouncer (`postgres://user:password@host:port/db`); обязателен.
    pub url: String,
    /// Адрес PostgreSQL в обход PgBouncer для миграций; по умолчанию — `url`.
    pub direct_url: Option<String>,
    /// Соединение через PgBouncer в режиме transaction.
    pub pgbouncer: bool,
    /// Наибольшее число соединений пула узла.
    pub max_connections: u32,
    /// Число соединений, поддерживаемых открытыми.
    pub min_connections: u32,
    /// Ожидание свободного соединения; дольше — `Unavailable`.
    pub acquire_timeout: Duration,
    /// Простаивающее соединение закрывается через этот срок.
    pub idle_timeout: Option<Duration>,
    /// Соединение пересоздаётся через этот срок (переключение реплик, балансировка).
    pub max_lifetime: Option<Duration>,
    /// Подготовленных выражений в кэше соединения (без PgBouncer).
    pub statement_cache: usize,
    /// `application_name` соединений (`pg_stat_activity`).
    pub application_name: String,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            direct_url: None,
            pgbouncer: false,
            max_connections: 32,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(5),
            idle_timeout: Some(Duration::from_secs(600)),
            max_lifetime: Some(Duration::from_secs(1800)),
            statement_cache: 256,
            application_name: "thepost".into(),
        }
    }
}

impl fmt::Debug for PoolConfig {
    // Адреса содержат пароль — в журнал не попадают.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolConfig")
            .field("pgbouncer", &self.pgbouncer)
            .field("max_connections", &self.max_connections)
            .field("min_connections", &self.min_connections)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_lifetime", &self.max_lifetime)
            .field("statement_cache", &self.statement_cache)
            .field("application_name", &self.application_name)
            .finish_non_exhaustive()
    }
}

impl PoolConfig {
    /// Параметры соединения с `url`; `pgbouncer` отключает кэш подготовленных выражений.
    pub fn connect_options(&self, url: &str, pgbouncer: bool) -> Result<PgConnectOptions> {
        if url.is_empty() {
            return Err(Error::invalid("database.url не задан"));
        }
        // Текст ошибки разбора может содержать адрес с паролем.
        let opts = PgConnectOptions::from_str(url)
            .map_err(|_| Error::invalid("database.url: неверный адрес"))?
            .application_name(&self.application_name);
        Ok(if pgbouncer {
            opts.statement_cache_capacity(0)
        } else {
            opts.statement_cache_capacity(self.statement_cache)
        })
    }

    fn pool_options(&self) -> PgPoolOptions {
        PgPoolOptions::new()
            .max_connections(self.max_connections)
            .min_connections(self.min_connections)
            .acquire_timeout(self.acquire_timeout)
            .idle_timeout(self.idle_timeout)
            .max_lifetime(self.max_lifetime)
    }
}

/// Пул по конфигурации; первое соединение открывается сразу (ошибка адреса или доступа — при старте).
pub async fn connect(cfg: &PoolConfig) -> Result<PgPool> {
    let opts = cfg.connect_options(&cfg.url, cfg.pgbouncer)?;
    cfg.pool_options().connect_with(opts).await.map_err(db_err)
}

/// Пул по конфигурации с соединениями по требованию (роли загружаются лениво, SRS 2.7).
pub fn connect_lazy(cfg: &PoolConfig, url: &str) -> Result<PgPool> {
    let opts = cfg.connect_options(url, cfg.pgbouncer)?;
    Ok(cfg.pool_options().connect_lazy_with(opts))
}

/// Пул из одного соединения в обход PgBouncer для миграций ([`crate::schema::migrate`], модули).
pub async fn connect_direct(cfg: &PoolConfig) -> Result<PgPool> {
    let (url, pgbouncer) = match &cfg.direct_url {
        Some(url) => (url.as_str(), false),
        None => (cfg.url.as_str(), cfg.pgbouncer),
    };
    let opts = cfg.connect_options(url, pgbouncer)?;
    PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(cfg.acquire_timeout)
        .connect_with(opts)
        .await
        .map_err(db_err)
}

/// Будущее тела транзакции, заимствующее соединение.
pub type TxFuture<'c, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'c>>;

/// Выполняет `f` в транзакции организации (`SET LOCAL app.tenant_id`, RLS): `Ok` — фиксация,
/// ошибка — откат. Внутри — только СУБД: к BLOB и сети в открытой транзакции не обращаться.
///
/// ```no_run
/// # async fn demo(pool: &sqlx::PgPool, tenant: tp_contracts::TenantId) -> tp_contracts::Result<()> {
/// let n: i64 = tp_meta::pool::with_tenant(pool, tenant, |tx| {
///     Box::pin(async move {
///         sqlx::query_scalar("SELECT count(*) FROM accounts")
///             .fetch_one(&mut *tx)
///             .await
///             .map_err(tp_meta::schema::db_err)
///     })
/// })
/// .await?;
/// # Ok(()) }
/// ```
pub async fn with_tenant<T, F>(pool: &PgPool, tenant: TenantId, f: F) -> Result<T>
where
    F: for<'c> FnOnce(&'c mut PgConnection) -> TxFuture<'c, T>,
{
    let mut tx = begin(pool, tenant).await?;
    let out = f(&mut tx).await?;
    tx.commit().await.map_err(db_err)?;
    Ok(out)
}

/// Как [`with_tenant`], но по всем организациям (`SET LOCAL app.all_tenants`): GC, журнал событий.
pub async fn with_system<T, F>(pool: &PgPool, f: F) -> Result<T>
where
    F: for<'c> FnOnce(&'c mut PgConnection) -> TxFuture<'c, T>,
{
    let mut tx = begin_system(pool).await?;
    let out = f(&mut tx).await?;
    tx.commit().await.map_err(db_err)?;
    Ok(out)
}
