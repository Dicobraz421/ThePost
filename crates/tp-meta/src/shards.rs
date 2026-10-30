//! Справочник шардов: учётная запись → пул PostgreSQL её шарда (SRS 3.6).
//!
//! Каталог (организации, домены, учётные записи, справочник `shards`) — в основной БД; учётная запись
//! хранит `shard_id`. В базовой поставке один шард `0` — сама основная БД (`dsn IS NULL`).
//! Вызывающие получают пул только через [`ShardResolver`]: модуль «Шардирование» добавляет шарды
//! строками справочника или своей реализацией резолвера, не меняя кода вызывающих.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use async_trait::async_trait;
use sqlx::{FromRow, PgPool};
use tp_contracts::meta::AccountScope;
use tp_contracts::{Error, Result, TenantId};

use crate::pool::{PoolConfig, connect_lazy};
use crate::schema::{begin, db_err};

/// Номер шарда.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardId(pub i32);

impl ShardId {
    /// Шард базовой поставки — основная БД.
    pub const LOCAL: ShardId = ShardId(0);
}

/// Запись справочника шардов.
#[derive(Clone, PartialEq, Eq, FromRow)]
pub struct Shard {
    /// Номер.
    #[sqlx(rename = "shard_id", try_from = "i32")]
    pub id: ShardId,
    /// Имя.
    pub name: String,
    /// Адрес кластера PostgreSQL; `None` — основная БД.
    pub dsn: Option<String>,
    /// Принимает новые учётные записи.
    pub accepts_new: bool,
}

impl From<i32> for ShardId {
    fn from(v: i32) -> Self {
        ShardId(v)
    }
}

impl fmt::Debug for Shard {
    // Адрес содержит пароль — в журнал не попадает.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shard")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("local", &self.dsn.is_none())
            .field("accepts_new", &self.accepts_new)
            .finish()
    }
}

/// Разрешение учётной записи в пул её шарда — точка расширения модуля «Шардирование».
#[async_trait]
pub trait ShardResolver: Send + Sync + 'static {
    /// Шард учётной записи; `NotFound` — учётной записи нет.
    async fn shard_of(&self, scope: AccountScope) -> Result<ShardId>;

    /// Пул шарда; `NotFound` — шарда нет в справочнике.
    async fn pool(&self, shard: ShardId) -> Result<PgPool>;

    /// Шард для новой учётной записи организации.
    async fn place(&self, tenant: TenantId) -> Result<ShardId>;

    /// Пул шарда учётной записи.
    async fn pool_for(&self, scope: AccountScope) -> Result<PgPool> {
        let shard = self.shard_of(scope).await?;
        self.pool(shard).await
    }
}

/// Справочник шардов в основной БД; пулы шардов создаются по требованию и кэшируются.
pub struct ShardDirectory {
    catalog: PgPool,
    cfg: PoolConfig,
    pools: Mutex<HashMap<ShardId, PgPool>>,
}

impl fmt::Debug for ShardDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShardDirectory").finish_non_exhaustive()
    }
}

impl ShardDirectory {
    /// Справочник над пулом основной БД; пулы других шардов — с параметрами `cfg` (адрес — `dsn` шарда).
    pub fn new(catalog: PgPool, cfg: PoolConfig) -> Self {
        Self {
            catalog,
            cfg,
            pools: Mutex::new(HashMap::new()),
        }
    }

    /// Все шарды по номеру.
    pub async fn list(&self) -> Result<Vec<Shard>> {
        sqlx::query_as("SELECT shard_id, name, dsn, accepts_new FROM shards ORDER BY shard_id")
            .fetch_all(&self.catalog)
            .await
            .map_err(db_err)
    }

    /// Забывает пул шарда (после смены адреса): следующий запрос создаст пул заново.
    pub fn forget(&self, shard: ShardId) {
        self.cached().remove(&shard);
    }

    fn cached(&self) -> std::sync::MutexGuard<'_, HashMap<ShardId, PgPool>> {
        // Карта пулов согласована при любой панике держателя: вставка — одна операция.
        self.pools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl ShardResolver for ShardDirectory {
    async fn shard_of(&self, scope: AccountScope) -> Result<ShardId> {
        let mut tx = begin(&self.catalog, scope.tenant).await?;
        let shard: Option<i32> = sqlx::query_scalar(
            "SELECT shard_id FROM accounts WHERE tenant_id = $1 AND account_id = $2",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        shard.map(ShardId).ok_or(Error::NotFound)
    }

    async fn pool(&self, shard: ShardId) -> Result<PgPool> {
        if let Some(p) = self.cached().get(&shard) {
            return Ok(p.clone());
        }
        let dsn: Option<Option<String>> =
            sqlx::query_scalar("SELECT dsn FROM shards WHERE shard_id = $1")
                .bind(shard.0)
                .fetch_optional(&self.catalog)
                .await
                .map_err(db_err)?;
        let pool = match dsn.ok_or(Error::NotFound)? {
            None => self.catalog.clone(),
            Some(dsn) => connect_lazy(&self.cfg, &dsn)?,
        };
        // Параллельный запрос мог создать пул раньше — остаётся первый.
        Ok(self.cached().entry(shard).or_insert(pool).clone())
    }

    async fn place(&self, _tenant: TenantId) -> Result<ShardId> {
        // Базовая поставка: первый шард, принимающий новые учётные записи.
        let shard: Option<i32> = sqlx::query_scalar(
            "SELECT shard_id FROM shards WHERE accepts_new ORDER BY shard_id LIMIT 1",
        )
        .fetch_optional(&self.catalog)
        .await
        .map_err(db_err)?;
        shard
            .map(ShardId)
            .ok_or_else(|| Error::internal("нет шарда, принимающего новые учётные записи"))
    }
}
