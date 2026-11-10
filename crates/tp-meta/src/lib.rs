//! Контур метаданных PostgreSQL: схема, миграции, реализации хранилищ (SRS 3.1, 3.4, 3.6).
//!
//! [`PgMeta`] — одна реализация трейтов `tp-contracts` над пулом PostgreSQL. Каждая операция —
//! короткая транзакция в контексте организации ([`schema::begin`]); к BLOB и сети внутри транзакций
//! реализация не обращается.

pub mod abuse;
pub mod accounts;
pub mod acl;
pub mod app_passwords;
pub mod blob_index;
pub mod changelog;
pub mod credentials;
pub mod delivery;
pub mod doctor;
pub mod events;
pub mod failover;
pub mod groups;
pub mod index_manifest;
pub mod lease;
pub mod mailboxes;
pub mod membership;
pub mod messages;
pub mod modules;
pub mod pool;
pub mod queue;
pub mod retention;
pub mod schema;
pub mod shards;
pub mod signals;
pub mod transport;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use tp_contracts::bus::{Event, EventBus, EventKind};
use tp_contracts::config::duration;
use tp_contracts::events::partition_of;
use tp_contracts::{AccountId, Result, TenantId};

use crate::abuse::AbuseBlocksConfig;
use crate::delivery::DeliveryLogConfig;
use crate::doctor::DoctorConfig;
use crate::events::EventsConfig;
use crate::failover::Failover;
use crate::pool::TxFuture;
use crate::retention::RetentionConfig;
use crate::signals::SignalsConfig;

/// Секция конфигурации контура метаданных `meta` (SRS 5.5); пул соединений — отдельная секция
/// `database` ([`pool::PoolConfig`]). Все поля необязательны; неизвестное поле — ошибка разбора.
///
/// ```yaml
/// meta:
///   blob_tombstone_ttl: 24h
///   events:
///     lease_ttl: 30s
///     node_ttl: 15s
///   doctor:
///     timeout: 10s
///   retention:
///     trash_retention: 30d
///   delivery:
///     retention: 30d
///   signals:
///     channel: tp_signals
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MetaConfig {
    /// Применять миграции схемы ядра при старте ([`PgMeta::open`]). Требует перезапуска.
    pub migrate_on_start: bool,
    /// Срок хранения надгробия BLOB после `finalize_delete`: загрузка, начатая до удаления объекта,
    /// получит `Bumped`. Больше максимальной длительности загрузки объекта (SRS 3.4).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub blob_tombstone_ttl: Duration,
    /// Число разделов журнала событий ядра; постоянно для инсталляции (смена — миграция журнала).
    /// Требует перезапуска.
    pub event_partitions: u32,
    /// Подписчики журнала событий и членство узлов.
    pub events: EventsConfig,
    /// Пороги проверок СУБД `thepost doctor`.
    pub doctor: DoctorConfig,
    /// Значения по умолчанию политик хранения: срок корзины, квота.
    pub retention: RetentionConfig,
    /// Журнал доставки: срок хранения.
    pub delivery: DeliveryLogConfig,
    /// Сигналы кластера через LISTEN/NOTIFY ([`signals::PgSignalBus`]).
    pub signals: SignalsConfig,
    /// Блокировки источников и учётных записей ([`abuse`]).
    pub abuse: AbuseBlocksConfig,
}

impl MetaConfig {
    /// Имя секции в config.yaml.
    pub const SECTION: &'static str = "meta";
}

impl Default for MetaConfig {
    fn default() -> Self {
        Self {
            migrate_on_start: true,
            blob_tombstone_ttl: Duration::from_secs(24 * 3600),
            event_partitions: 64,
            events: EventsConfig::default(),
            doctor: DoctorConfig::default(),
            retention: RetentionConfig::default(),
            delivery: DeliveryLogConfig::default(),
            signals: SignalsConfig::default(),
            abuse: AbuseBlocksConfig::default(),
        }
    }
}

/// Шина, в которую хранилища публикуют сигналы после фиксации.
#[derive(Clone, Default)]
struct Signals(Option<Arc<dyn EventBus>>);

impl std::fmt::Debug for Signals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Signals(bus)"
        } else {
            "Signals(none)"
        })
    }
}

/// Хранилища метаданных на PostgreSQL.
///
/// С шиной ([`PgMeta::with_bus`]) после фиксации публикуются сигналы: `LogAppended` — после записи событий
/// в журнал ядра, `MembersChanged` — при изменении состава кластера, `Blocked` — после блокировки,
/// `Unblocked` — после снятия блокировки, `TransportChanged` — после изменения настроек транспорта ([`transport`]). Сигнал
/// at-most-once: ошибка публикации не отменяет зафиксированное изменение (только запись в журнал узла).
///
/// Над [`Failover`] ([`PgMeta::from_failover`]) хранилища переживают переключение СУБД: при временном отказе
/// операция повторяется с нарастающей паузой до `database.failover.retry_for` (поиск primary и сброс пула —
/// [`failover`]). Повторяются чтения, записи с ключом и записи, идемпотентные по сути:
/// - **с ключом** (исход обрыва во время COMMIT — по ключу, [`Failover::with_tenant_keyed`], META-40): вставка
///   письма по его id (`MessageStore::insert`, `insert_batch`; ключ — запись `created` журнала изменений),
///   журнал доставки (`DeliveryLog::record*`; ключ — номера `seq`, выданные до транзакции), очередь
///   модерации (`GroupStore::hold`; ключ — `HeldId` вызывающего), позиция подписчика (`EventLog::commit`;
///   ключ — позиция не меньше заданной);
/// - **идемпотентные по сути** — список у [`PgMeta::rewrite`].
///
/// Остальные записи выполняются один раз: временный отказ (`Unavailable` с `retry_after`) отдаётся
/// вызывающему, а следующая операция идёт к найденному primary.
#[derive(Debug, Clone)]
pub struct PgMeta {
    pool: PgPool,
    cfg: Arc<MetaConfig>,
    bus: Signals,
    failover: Option<Failover>,
}

impl PgMeta {
    /// Хранилища над готовым пулом; схема должна быть применена.
    pub fn new(pool: PgPool, cfg: MetaConfig) -> Self {
        Self {
            pool,
            cfg: Arc::new(cfg),
            bus: Signals::default(),
            failover: None,
        }
    }

    /// Хранилища над пулом `failover` с повтором чтений и записей с ключом или идемпотентных при переключении
    /// СУБД; схема должна быть применена.
    pub fn from_failover(failover: &Failover, cfg: MetaConfig) -> Self {
        Self {
            failover: Some(failover.clone()),
            ..Self::new(failover.pool().clone(), cfg)
        }
    }

    /// Как [`from_failover`](Self::from_failover); при `migrate_on_start` — применение миграций ядра.
    pub async fn open_failover(failover: &Failover, cfg: MetaConfig) -> Result<Self> {
        if cfg.migrate_on_start {
            schema::migrate(failover.pool()).await?;
        }
        Ok(Self::from_failover(failover, cfg))
    }

    /// **Идемпотентная** операция (чтение): над [`Failover`] временный отказ — повтор с нарастающей паузой
    /// ([`Failover::retry`]), иначе — один раз.
    pub(crate) async fn idempotent<T, F, Fut>(&self, mut op: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        match &self.failover {
            Some(fo) => fo.retry(|_| op()).await,
            None => op().await,
        }
    }

    /// Запись, **идемпотентная по сути**: повторное применение после обрыва во время COMMIT даёт то же
    /// состояние, поэтому над [`Failover`] она повторяется, как чтение ([`Failover::retry`]). Список с
    /// обоснованием (META-40):
    /// - upsert по значению: `AbuseBlocks::block` (срок — `greatest`, действующая блокировка не
    ///   сокращается), `AccountStore::upsert` (поиск по имени входа, системные папки — `ensure_system`),
    ///   `AclStore::set_acl`, `set_send_delegation`, `GroupStore::put` (группа заменяется целиком),
    ///   `Membership::heartbeat` (`upsert_node`), `set_policy`, `set_user_trash_purge`;
    /// - `set_*` по значению: `MailboxStore::set_subscribed` (повтор лишь добавляет запись `updated` в
    ///   журнал изменений), `AppPasswords::revoke` (`COALESCE` — время первого отзыва),
    ///   `mark_blob_damaged` (`coalesce` — время первой пометки);
    /// - удаление по ключу без возврата удалённого: `AbuseBlocks::unblock`, `Membership::leave`,
    ///   `DeliveryLog::purge` (секции суток), `purge_events` (число удалённых при повторе — только
    ///   оставшиеся).
    ///
    /// Не повторяются: CAS по версии (`transport`, `put_manifest`) и номеру захвата (лизы, очередь) —
    /// повтор применённой записи отвечает `Conflict`; счётчики ссылок BLOB, UID и `ModSeq` писем,
    /// выдача идентификаторов внутри операции (`enqueue`, `create`, `copy`), удаление с возвратом
    /// удалённого (`take_held`, `GroupStore::remove`) — повтор вернул бы другое.
    pub(crate) async fn rewrite<T, F, Fut>(&self, op: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.idempotent(op).await
    }

    /// Запись с ключом в транзакции организации `tenant`: над [`Failover`] — повтор с установлением исхода
    /// обрыва во время COMMIT проверкой `committed` ([`Failover::with_tenant_keyed`]), иначе — один раз.
    pub(crate) async fn keyed<T, F, C>(&self, tenant: TenantId, f: F, committed: C) -> Result<T>
    where
        F: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, T>,
        C: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, Option<T>>,
    {
        match &self.failover {
            Some(fo) => fo.with_tenant_keyed(tenant, f, committed).await,
            None => pool::with_tenant(&self.pool, tenant, f).await,
        }
    }

    /// Как [`keyed`](Self::keyed), но по всем организациям ([`Failover::with_system_keyed`]).
    pub(crate) async fn keyed_system<T, F, C>(&self, f: F, committed: C) -> Result<T>
    where
        F: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, T>,
        C: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, Option<T>>,
    {
        match &self.failover {
            Some(fo) => fo.with_system_keyed(f, committed).await,
            None => pool::with_system(&self.pool, f).await,
        }
    }

    /// Неидемпотентная операция (запись без ключа): один раз; над [`Failover`] временный отказ — проверка
    /// primary и `Unavailable` с `retry_after` ([`Failover::once`]).
    pub(crate) async fn once<T>(&self, op: impl Future<Output = Result<T>>) -> Result<T> {
        match &self.failover {
            Some(fo) => fo.once(op).await,
            None => op.await,
        }
    }

    /// Те же хранилища с публикацией сигналов в `bus` после фиксации.
    pub fn with_bus(mut self, bus: Arc<dyn EventBus>) -> Self {
        self.bus = Signals(Some(bus));
        self
    }

    /// Публикует сигнал после фиксации; ошибка — только в журнал узла (сигнал at-most-once).
    pub(crate) async fn signal(&self, event: Event) {
        if let Some(bus) = &self.bus.0
            && let Err(e) = bus.publish(event).await
        {
            tracing::warn!(error = %e, "сигнал не опубликован");
        }
    }

    /// Сигналы `LogAppended` разделов журнала событий учётных записей `accounts` (события записаны и
    /// зафиксированы).
    pub(crate) async fn signal_log(&self, accounts: impl IntoIterator<Item = AccountId>) {
        if self.bus.0.is_none() {
            return;
        }
        let mut parts: Vec<_> = accounts
            .into_iter()
            .map(|a| partition_of(a, self.cfg.event_partitions))
            .collect();
        parts.sort();
        parts.dedup();
        for partition in parts {
            self.signal(Event {
                tenant: TenantId::DEFAULT,
                account: None,
                kind: EventKind::LogAppended { partition },
            })
            .await;
        }
    }

    /// Хранилища над пулом; при `migrate_on_start` — применение миграций ядра.
    pub async fn open(pool: PgPool, cfg: MetaConfig) -> Result<Self> {
        if cfg.migrate_on_start {
            schema::migrate(&pool).await?;
        }
        Ok(Self::new(pool, cfg))
    }

    /// Пул соединений.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde::Deserialize;
    use tp_contracts::config::duration::PATTERN;

    use super::MetaConfig;
    use crate::delivery::DeliveryLogConfig;
    use crate::doctor::DoctorConfig;
    use crate::events::EventsConfig;
    use crate::pool::PoolConfig;

    /// Секции tp-meta в config.yaml (так их разбирает приложение — по имени секции).
    #[derive(Debug, Default, Deserialize)]
    #[serde(default)]
    struct Sections {
        database: PoolConfig,
        meta: MetaConfig,
    }

    fn parse(yaml: &str) -> Result<Sections, serde_yaml_ng::Error> {
        serde_yaml_ng::from_str(yaml)
    }

    #[test]
    fn sections_from_yaml() {
        let s = parse(
            r#"
database:
  url: postgres://thepost@db.internal/thepost
  password: { secret: "thepost/db#password" }
  acquire_timeout: 5s
  idle_timeout: null
  max_lifetime: 1h30m
meta:
  blob_tombstone_ttl: 12h
  events:
    lease_ttl: 30s
    node_ttl: 9s
  doctor:
    timeout: 250ms
"#,
        )
        .expect("yaml");
        assert_eq!(s.database.acquire_timeout, Duration::from_secs(5));
        assert_eq!(s.database.idle_timeout, None);
        assert_eq!(s.database.max_lifetime, Some(Duration::from_secs(5400)));
        assert_eq!(
            s.database
                .password
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("thepost/db#password")
        );
        assert_eq!(
            s.database.max_connections,
            PoolConfig::default().max_connections
        );
        assert_eq!(s.meta.blob_tombstone_ttl, Duration::from_secs(12 * 3600));
        assert_eq!(s.meta.events.lease_ttl, Duration::from_secs(30));
        assert_eq!(s.meta.events.node_ttl, Duration::from_secs(9));
        assert_eq!(s.meta.events.heartbeat_interval(), Duration::from_secs(3));
        assert_eq!(s.meta.events.batch, EventsConfig::default().batch);
        assert_eq!(s.meta.doctor.timeout, Duration::from_millis(250));
        assert!(s.database.check().is_ok());
    }

    #[test]
    fn empty_sections_are_default() {
        let s = parse("database: {}\nmeta: {}\n").expect("yaml");
        assert_eq!(s.database, PoolConfig::default());
        assert_eq!(s.meta, MetaConfig::default());
        let s = parse("meta:\n  events: {}\n  doctor: {}\n").expect("yaml");
        assert_eq!(s.meta.events, EventsConfig::default());
        assert_eq!(s.meta.doctor, DoctorConfig::default());
        assert_eq!(s.meta.delivery, DeliveryLogConfig::default());
        let s = parse("meta:\n  delivery:\n    retention: 7d\n").expect("yaml");
        assert_eq!(
            s.meta.delivery.retention,
            Duration::from_secs(7 * 24 * 3600)
        );
        // Без секций — тоже значения по умолчанию.
        let s = parse("{}").expect("yaml");
        assert_eq!(s.meta, MetaConfig::default());
    }

    #[test]
    fn strict_parsing() {
        let err = |yaml: &str| parse(yaml).expect_err(yaml).to_string();
        // Число без единицы и другие записи длительности — ошибка.
        for bad in ["5", "5 sec", "-1s", "5S"] {
            let e = err(&format!("database:\n  acquire_timeout: {bad}\n"));
            assert!(
                e.contains("acquire_timeout") || e.contains("duration"),
                "{bad}: {e}"
            );
        }
        assert!(err("meta:\n  events:\n    lease_ttl: 30\n").contains("duration"));
        // Неизвестные поля не теряются: строгая проверка их отвергает.
        assert!(err("database:\n  max_conections: 3\n").contains("max_conections"));
        assert!(err("meta:\n  events:\n    lease: 30s\n").contains("lease"));
        assert!(err("meta:\n  doctor:\n    timeot: 1s\n").contains("timeot"));
        // Профиль задаёт приложение, не секция.
        assert!(err("database:\n  profile: dev\n").contains("profile"));
        // Пароль открытым текстом на месте ссылки — ошибка без значения.
        let e = err("database:\n  password: hunter2\n");
        assert!(!e.contains("hunter2"), "{e}");
    }

    #[test]
    fn json_schema_durations() {
        let schema = serde_json::to_value(schemars::schema_for!(PoolConfig)).expect("schema");
        let timeout = &schema["properties"]["acquire_timeout"];
        assert_eq!(timeout["type"], "string", "{timeout}");
        assert_eq!(timeout["pattern"], PATTERN);
        assert!(schema["properties"].get("profile").is_none());
        let meta = serde_json::to_value(schemars::schema_for!(MetaConfig)).expect("schema");
        assert_eq!(meta["additionalProperties"], false, "{meta}");
    }
}
