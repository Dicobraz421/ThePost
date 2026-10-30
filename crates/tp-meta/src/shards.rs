//! Справочник шардов: учётная запись → пул PostgreSQL её шарда (SRS 3.6).
//!
//! Каталог (организации, домены, учётные записи, справочник `shards`) — в основной БД; учётная запись
//! хранит `shard_id`. В базовой поставке один шард `0` — сама основная БД (`dsn IS NULL`).
//! Вызывающие получают пул только через [`ShardResolver`]: модуль «Шардирование» добавляет шарды
//! строками справочника или своей реализацией резолвера, не меняя кода вызывающих.
//!
//! Справочник хранит адрес шарда без пароля и ссылку на секрет с паролем (`credentials`, CTR-15,
//! решение 2026-10-30): пароль читается из [`SecretStore`] при создании пула шарда. Адрес с паролем
//! отклоняет ограничение таблицы и проверка при чтении справочника.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sqlx::{FromRow, PgPool};
use tp_contracts::meta::AccountScope;
use tp_contracts::secrets::{SecretRef, SecretStore};
use tp_contracts::{Error, Result, TenantId};

use crate::pool::{PoolConfig, connect_lazy, fetch_secret, url_has_password};
use crate::schema::{begin, db_err};

/// Номер шарда.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardId(pub i32);

impl ShardId {
    /// Шард базовой поставки — основная БД.
    pub const LOCAL: ShardId = ShardId(0);
}

impl From<i32> for ShardId {
    fn from(v: i32) -> Self {
        ShardId(v)
    }
}

/// Запись справочника шардов.
#[derive(Clone, PartialEq, Eq)]
pub struct Shard {
    /// Номер.
    pub id: ShardId,
    /// Имя.
    pub name: String,
    /// Адрес кластера PostgreSQL без пароля; `None` — основная БД.
    pub dsn: Option<String>,
    /// Пароль роли шарда — ссылка на секрет в хранилище; `None` — вход без пароля (сертификат клиента).
    pub credentials: Option<SecretRef>,
    /// Принимает новые учётные записи.
    pub accepts_new: bool,
}

impl fmt::Debug for Shard {
    // Адрес в журнал не попадает (прежние строки справочника могли хранить пароль).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shard")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("local", &self.dsn.is_none())
            .field("credentials", &self.credentials)
            .field("accepts_new", &self.accepts_new)
            .finish()
    }
}

#[derive(FromRow)]
struct ShardRow {
    shard_id: i32,
    name: String,
    dsn: Option<String>,
    credentials: Option<String>,
    accepts_new: bool,
}

impl TryFrom<ShardRow> for Shard {
    type Error = Error;

    /// Проверка при чтении: адрес без пароля, ссылка на секрет разбирается.
    fn try_from(r: ShardRow) -> Result<Self> {
        let id = r.shard_id;
        if r.dsn.as_deref().is_some_and(url_has_password) {
            tracing::error!(shard = id, "справочник шардов: пароль в адресе шарда");
            return Err(Error::invalid(format!(
                "шард {id}: пароль в адресе запрещён; пароль — ссылка на секрет в столбце credentials"
            )));
        }
        let credentials = r
            .credentials
            .as_deref()
            .map(SecretRef::from_str)
            .transpose()
            .map_err(|_| Error::invalid(format!("шард {id}: неверная ссылка на секрет")))?;
        Ok(Shard {
            id: ShardId(id),
            name: r.name,
            dsn: r.dsn,
            credentials,
            accepts_new: r.accepts_new,
        })
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
    secrets: Arc<dyn SecretStore>,
    pools: Mutex<HashMap<ShardId, PgPool>>,
}

impl fmt::Debug for ShardDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShardDirectory").finish_non_exhaustive()
    }
}

impl ShardDirectory {
    /// Справочник над пулом основной БД; пулы других шардов — с параметрами `cfg` (адрес — `dsn` шарда,
    /// пароль — секрет `credentials` из `secrets`; `cfg.url` и `cfg.password` не используются).
    pub fn new(catalog: PgPool, cfg: PoolConfig, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            catalog,
            cfg,
            secrets,
            pools: Mutex::new(HashMap::new()),
        }
    }

    /// Все шарды по номеру; строка с паролем в адресе или неверной ссылкой на секрет — `Invalid`.
    pub async fn list(&self) -> Result<Vec<Shard>> {
        let rows: Vec<ShardRow> = sqlx::query_as(
            "SELECT shard_id, name, dsn, credentials, accepts_new FROM shards ORDER BY shard_id",
        )
        .fetch_all(&self.catalog)
        .await
        .map_err(db_err)?;
        rows.into_iter().map(Shard::try_from).collect()
    }

    /// Забывает пул шарда (смена адреса или секрета): следующий запрос перечитает справочник и секрет
    /// и создаст пул заново. Соединения прежнего пула закрываются, когда его отпустят все держатели.
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

    /// Пул шарда. Секрет недоступен, отозван или запрещён политикой — `Unavailable` (причина — в журнале).
    async fn pool(&self, shard: ShardId) -> Result<PgPool> {
        if let Some(p) = self.cached().get(&shard) {
            return Ok(p.clone());
        }
        let row: Option<ShardRow> = sqlx::query_as(
            "SELECT shard_id, name, dsn, credentials, accepts_new FROM shards WHERE shard_id = $1",
        )
        .bind(shard.0)
        .fetch_optional(&self.catalog)
        .await
        .map_err(db_err)?;
        let row = Shard::try_from(row.ok_or(Error::NotFound)?)?;
        let pool = match &row.dsn {
            None => self.catalog.clone(),
            Some(dsn) => {
                // Секрет читается вне транзакций БД.
                let password = match &row.credentials {
                    Some(r) => Some(
                        fetch_secret(self.secrets.as_ref(), r, &format!("шард {}", shard.0))
                            .await?,
                    ),
                    None => None,
                };
                connect_lazy(&self.cfg, dsn, password.as_ref())?
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(dsn: Option<&str>, credentials: Option<&str>) -> ShardRow {
        ShardRow {
            shard_id: 7,
            name: "s7".into(),
            dsn: dsn.map(Into::into),
            credentials: credentials.map(Into::into),
            accepts_new: false,
        }
    }

    #[test]
    fn read_check_rejects_password_in_dsn() {
        // Прежние строки справочника (до ограничения NOT VALID) с паролем в адресе.
        for dsn in [
            "postgres://u:hunter2@db/shard",
            "postgres://u@db/shard?sslmode=verify-full&password=hunter2",
        ] {
            match Shard::try_from(row(Some(dsn), None)) {
                Err(Error::Invalid(m)) => assert!(!m.contains("hunter2"), "{m}"),
                other => panic!("{dsn}: {other:?}"),
            }
        }
        let ok = Shard::try_from(row(
            Some("postgres://u@db:5432/shard"),
            Some("thepost/shard7#password"),
        ))
        .expect("shard");
        assert_eq!(
            ok.credentials.as_ref().map(ToString::to_string).as_deref(),
            Some("thepost/shard7#password")
        );
        assert!(!format!("{ok:?}").contains("db:5432"));
        assert!(matches!(
            Shard::try_from(row(Some("postgres://db/s"), Some("hunter2"))),
            Err(Error::Invalid(m)) if !m.contains("hunter2")
        ));
        assert!(Shard::try_from(row(None, None)).is_ok());
    }
}
