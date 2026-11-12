//! Справочник шардов: учётная запись → пул PostgreSQL её шарда (SRS 3.6, решение 2026-11-01-scale-shards-geo-admin).
//!
//! Каталог (организации, домены, учётные записи, справочник `shards`) — в основной БД; учётная запись
//! хранит `shard_id`. В базовой поставке один шард `0` — сама основная БД (`dsn IS NULL`).
//! Вызывающие получают пул только через [`ShardResolver`]: модуль «Шардирование» добавляет шарды
//! строками справочника или своей реализацией резолвера, не меняя кода вызывающих.
//!
//! Хранилища ящика (папки, письма, журнал изменений, ACL, корзина, манифест индекса) [`crate::PgMeta`]
//! получает через резолвер ([`crate::PgMeta::with_shards`]); один шард — вырожденный случай: список шардов
//! кэшируется, и при единственном шарде `0` запросов к справочнику на операцию нет.
//!
//! Кэш [`ShardDirectory`] (список шардов, учётная запись → шард, пулы) сбрасывает сигнал изменения
//! справочника ([`ShardDirectory::follow`]); пулы создаются заново лениво — при первом запросе к шарду.
//!
//! Реплика шарда для фоновых чтений (`ReadIntent::Stale`, META-32) — столбец `read_replica` (без пароля,
//! учётные данные — как у шарда); отставание проверяет [`crate::replica`].
//!
//! Справочник хранит адрес шарда без пароля и ссылку на секрет с паролем (`credentials`, CTR-15,
//! решение 2026-10-30): пароль читается из [`SecretStore`] при создании пула шарда. Адрес с паролем
//! отклоняет ограничение таблицы и проверка при чтении справочника.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use futures::StreamExt;
use sqlx::{FromRow, PgPool};
use tp_contracts::bus::{EventBus, EventKind};
use tp_contracts::meta::AccountScope;
use tp_contracts::secrets::{SecretRef, SecretStore};
use tp_contracts::{AccountId, Error, Result, TenantId};

use crate::pool::{PoolConfig, connect_lazy, fetch_secret, url_has_password};
use crate::replica::Replica;
use crate::schema::{begin, db_err};

/// Номер шарда — тип контракта (CTR-34): [`ShardId::MAIN`] — основная БД.
pub use tp_contracts::ShardId;

/// Назначение чтения (SRS 3.1): протокольные чтения и все записи — `Primary`; `Stale` — только закрытый
/// список фоновых чтений (экспорт, отчёты, eDiscovery, verify, «сироты» BLOB, первичная индексация).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ReadIntent {
    /// Primary шарда.
    #[default]
    Primary,
    /// Реплика шарда при допустимом отставании, иначе primary.
    Stale,
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
    /// Площадка (ЦОД) шарда; `None` — не задана.
    pub site: Option<String>,
    /// Адрес реплики шарда для фоновых чтений без пароля; `None` — фоновые чтения на primary.
    pub read_replica: Option<String>,
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
            .field("site", &self.site)
            .field("read_replica", &self.read_replica.is_some())
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
    site: Option<String>,
    read_replica: Option<String>,
}

impl TryFrom<ShardRow> for Shard {
    type Error = Error;

    /// Проверка при чтении: адрес без пароля, ссылка на секрет разбирается.
    fn try_from(r: ShardRow) -> Result<Self> {
        let id = r.shard_id;
        if r.dsn.as_deref().is_some_and(url_has_password)
            || r.read_replica.as_deref().is_some_and(url_has_password)
        {
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
            site: r.site,
            read_replica: r.read_replica,
        })
    }
}

/// Разрешение учётной записи в пул её шарда — точка расширения модуля «Шардирование».
#[async_trait]
pub trait ShardResolver: Send + Sync + 'static {
    /// Шард учётной записи; `NotFound` — учётной записи нет.
    async fn shard_of(&self, scope: AccountScope) -> Result<ShardId>;

    /// Пул шарда для операции с назначением `intent` (записи — всегда `Primary`); `NotFound` — шарда нет
    /// в справочнике.
    async fn pool(&self, shard: ShardId, intent: ReadIntent) -> Result<PgPool>;

    /// Шард для новой учётной записи организации.
    async fn place(&self, tenant: TenantId) -> Result<ShardId>;

    /// Все шарды справочника по возрастанию, всегда с [`ShardId::MAIN`]. Вызывается на каждую операцию
    /// ящика: реализация кэширует список до сигнала изменения справочника.
    async fn shards(&self) -> Result<Vec<ShardId>>;

    /// Сброс кэша по сигналу изменения справочника (шарды, адреса, размещение учётных записей).
    fn invalidate(&self) {}

    /// Реплика шарда для фоновых чтений; `None` — нет (чтения на primary).
    async fn replica(&self, shard: ShardId) -> Result<Option<Arc<Replica>>> {
        let _ = shard;
        Ok(None)
    }

    /// Пул шарда учётной записи.
    async fn pool_for(&self, scope: AccountScope, intent: ReadIntent) -> Result<PgPool> {
        let shard = self.shard_of(scope).await?;
        self.pool(shard, intent).await
    }
}

/// Предел кэша «учётная запись → шард»: при заполнении кэш сбрасывается целиком.
const ACCOUNT_CACHE: usize = 65_536;

#[derive(Default)]
struct Cache {
    /// Шарды справочника; `None` — не прочитаны.
    shards: Option<Vec<ShardId>>,
    accounts: HashMap<(TenantId, AccountId), ShardId>,
    pools: HashMap<ShardId, PgPool>,
    replicas: HashMap<ShardId, Option<Arc<Replica>>>,
    /// Поколение кэша: результат запроса, начатого до сброса, не кэшируется.
    epoch: u64,
}

/// Справочник шардов в основной БД; пулы шардов создаются по требованию и кэшируются.
pub struct ShardDirectory {
    catalog: PgPool,
    cfg: PoolConfig,
    secrets: Arc<dyn SecretStore>,
    cache: Mutex<Cache>,
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
            cache: Mutex::new(Cache::default()),
        }
    }

    /// Все шарды по номеру; строка с паролем в адресе или неверной ссылкой на секрет — `Invalid`.
    pub async fn list(&self) -> Result<Vec<Shard>> {
        let rows: Vec<ShardRow> = sqlx::query_as(
            "SELECT shard_id, name, dsn, credentials, accepts_new, site, read_replica FROM shards ORDER BY shard_id",
        )
        .fetch_all(&self.catalog)
        .await
        .map_err(db_err)?;
        rows.into_iter().map(Shard::try_from).collect()
    }

    /// Забывает пулы шарда (смена адреса или секрета): следующий запрос перечитает справочник и секрет
    /// и создаст пул заново. Соединения прежнего пула закрываются, когда его отпустят все держатели.
    pub fn forget(&self, shard: ShardId) {
        let mut c = self.cached();
        c.pools.remove(&shard);
        c.replicas.remove(&shard);
    }

    /// Сбрасывает кэш по сигналам системной подписки шины `bus`: `ConfigReloaded` (справочник — часть
    /// конфигурации инсталляции) и `Resync` (сигналы могли потеряться). Задача завершается вместе с потоком
    /// подписки (остановка шины).
    pub async fn follow(
        self: Arc<Self>,
        bus: Arc<dyn EventBus>,
    ) -> Result<tokio::task::JoinHandle<()>> {
        let mut stream = bus.subscribe_system().await?;
        Ok(tokio::spawn(async move {
            while let Some(event) = stream.next().await {
                if matches!(event.kind, EventKind::ConfigReloaded | EventKind::Resync) {
                    tracing::debug!("справочник шардов: сброс кэша по сигналу");
                    self.invalidate();
                }
            }
        }))
    }

    /// Секция пула для прямых соединений с шардом `shard` (слушатель и публикатор сигналов шарда): параметры
    /// справочника, адрес — `dsn` шарда, пароль — секрет `credentials`; без PgBouncer, списка адресов и
    /// динамических учётных данных. Шард 0 (основная БД) — `Invalid`: её секция — `database`.
    pub async fn shard_pool_config(&self, shard: ShardId) -> Result<PoolConfig> {
        let rec = self.row(shard).await?;
        let Some(dsn) = rec.shard.dsn else {
            return Err(Error::invalid(format!(
                "шард {}: основная БД — секция database",
                shard.0
            )));
        };
        Ok(PoolConfig {
            url: dsn,
            urls: Vec::new(),
            password: rec.shard.credentials,
            dynamic_credentials: None,
            direct_url: None,
            direct_urls: Vec::new(),
            read_replica: None,
            owner: None,
            pgbouncer: false,
            ..self.cfg.clone()
        })
    }

    /// Применяет миграции схемы ядра на каждом шарде справочника с адресом (основную БД мигрирует
    /// [`crate::schema::migrate`]): с `database.owner` — от владельца схемы (имя роли и пароль владельца,
    /// адрес шарда), иначе — с учётными данными шарда. Данные каталога на шардах не пишутся.
    pub async fn migrate(&self) -> Result<()> {
        for shard in self.list().await? {
            let Some(dsn) = &shard.dsn else {
                continue;
            };
            let opts = match &self.cfg.owner {
                Some(owner) => {
                    let password = match &owner.password {
                        Some(r) => Some(
                            fetch_secret(self.secrets.as_ref(), r, "database.owner.password")
                                .await?,
                        ),
                        None => None,
                    };
                    self.cfg
                        .open_options(dsn, false, password.as_ref())?
                        .username(&owner.user)
                }
                None => {
                    let password = match &shard.credentials {
                        Some(r) => Some(
                            fetch_secret(self.secrets.as_ref(), r, &format!("шард {}", shard.id.0))
                                .await?,
                        ),
                        None => None,
                    };
                    self.cfg.open_options(dsn, false, password.as_ref())?
                }
            };
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(self.cfg.acquire_timeout)
                .connect_with(opts)
                .await
                .map_err(db_err)?;
            let applied = crate::schema::migrate(&pool).await;
            pool.close().await;
            applied?;
            tracing::info!(shard = shard.id.0, "схема шарда применена");
        }
        Ok(())
    }

    fn cached(&self) -> MutexGuard<'_, Cache> {
        // Кэш согласован при любой панике держателя: каждая вставка — одна операция.
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Строка справочника шарда; `NotFound` — шарда нет.
    async fn row(&self, shard: ShardId) -> Result<ShardRecord> {
        let row: Option<ShardRow> = sqlx::query_as(
            "SELECT shard_id, name, dsn, credentials, accepts_new, site, read_replica FROM shards WHERE shard_id = $1",
        )
        .bind(shard.0)
        .fetch_optional(&self.catalog)
        .await
        .map_err(db_err)?;
        Ok(ShardRecord {
            shard: Shard::try_from(row.ok_or(Error::NotFound)?)?,
        })
    }

    /// Пул к адресу `dsn` шарда `shard` с паролем из секрета `credentials`.
    async fn connect(
        &self,
        shard: ShardId,
        dsn: &str,
        credentials: Option<&SecretRef>,
    ) -> Result<PgPool> {
        // Секрет читается вне транзакций БД.
        let password = match credentials {
            Some(r) => {
                Some(fetch_secret(self.secrets.as_ref(), r, &format!("шард {}", shard.0)).await?)
            }
            None => None,
        };
        connect_lazy(&self.cfg, dsn, password.as_ref())
    }

    /// Кэширует пул, если кэш не сброшен с поколения `epoch`; параллельный запрос мог создать пул раньше —
    /// остаётся первый.
    fn keep(&self, key: ShardId, epoch: u64, pool: PgPool) -> PgPool {
        let mut c = self.cached();
        if c.epoch != epoch {
            return pool;
        }
        c.pools.entry(key).or_insert(pool).clone()
    }
}

/// Прочитанная строка справочника.
struct ShardRecord {
    shard: Shard,
}

#[async_trait]
impl ShardResolver for ShardDirectory {
    async fn shard_of(&self, scope: AccountScope) -> Result<ShardId> {
        let key = (scope.tenant, scope.account);
        let epoch = {
            let c = self.cached();
            if let Some(s) = c.accounts.get(&key) {
                return Ok(*s);
            }
            c.epoch
        };
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
        let shard = shard.map(ShardId).ok_or(Error::NotFound)?;
        let mut c = self.cached();
        if c.epoch == epoch {
            if c.accounts.len() >= ACCOUNT_CACHE {
                c.accounts.clear();
            }
            c.accounts.insert(key, shard);
        }
        Ok(shard)
    }

    /// Пул шарда. Секрет недоступен, отозван или запрещён политикой — `Unavailable` (причина — в журнале).
    /// `Stale` — реплика шарда при допустимом отставании, иначе primary.
    async fn pool(&self, shard: ShardId, intent: ReadIntent) -> Result<PgPool> {
        if intent == ReadIntent::Stale
            && let Some(replica) = self.replica(shard).await?
            && replica.usable().await
        {
            return Ok(replica.pool().clone());
        }
        let key = shard;
        let epoch = {
            let c = self.cached();
            if let Some(p) = c.pools.get(&key) {
                return Ok(p.clone());
            }
            c.epoch
        };
        let rec = self.row(shard).await?;
        let pool = match &rec.shard.dsn {
            None => self.catalog.clone(),
            Some(dsn) => {
                self.connect(shard, dsn, rec.shard.credentials.as_ref())
                    .await?
            }
        };
        Ok(self.keep(key, epoch, pool))
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

    async fn shards(&self) -> Result<Vec<ShardId>> {
        let epoch = {
            let c = self.cached();
            if let Some(s) = &c.shards {
                return Ok(s.clone());
            }
            c.epoch
        };
        let mut ids: Vec<ShardId> =
            sqlx::query_scalar::<_, i32>("SELECT shard_id FROM shards ORDER BY shard_id")
                .fetch_all(&self.catalog)
                .await
                .map_err(db_err)?
                .into_iter()
                .map(ShardId)
                .collect();
        if !ids.contains(&ShardId::MAIN) {
            ids.insert(0, ShardId::MAIN);
            ids.sort();
        }
        let mut c = self.cached();
        if c.epoch == epoch {
            c.shards = Some(ids.clone());
        }
        Ok(ids)
    }

    async fn replica(&self, shard: ShardId) -> Result<Option<Arc<Replica>>> {
        let epoch = {
            let c = self.cached();
            if let Some(r) = c.replicas.get(&shard) {
                return Ok(r.clone());
            }
            c.epoch
        };
        let rec = self.row(shard).await?;
        let replica = match &rec.shard.read_replica {
            None => None,
            Some(url) => {
                let pool = self
                    .connect(shard, url, rec.shard.credentials.as_ref())
                    .await?;
                Some(Arc::new(Replica::new(pool, self.cfg.replica_max_lag)))
            }
        };
        let mut c = self.cached();
        if c.epoch != epoch {
            return Ok(replica);
        }
        Ok(c.replicas.entry(shard).or_insert(replica).clone())
    }

    fn invalidate(&self) {
        let mut c = self.cached();
        let epoch = c.epoch.wrapping_add(1);
        *c = Cache {
            epoch,
            ..Cache::default()
        };
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
            site: None,
            read_replica: None,
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
