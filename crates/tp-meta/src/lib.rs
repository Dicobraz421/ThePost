//! Контур метаданных PostgreSQL: схема, миграции, реализации хранилищ (SRS 3.1, 3.4, 3.6).
//!
//! [`PgMeta`] — одна реализация трейтов `tp-contracts` над пулом PostgreSQL. Каждая операция —
//! короткая транзакция в контексте организации ([`schema::begin`]); к BLOB и сети внутри транзакций
//! реализация не обращается.

pub mod accounts;
pub mod blob_index;
pub mod changelog;
pub mod doctor;
pub mod events;
pub mod lease;
pub mod mailboxes;
pub mod messages;
pub mod modules;
pub mod pool;
pub mod schema;
pub mod shards;

use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tp_contracts::Result;
use tp_contracts::config::duration;

use crate::doctor::DoctorConfig;
use crate::events::EventsConfig;

/// Секция конфигурации контура метаданных `meta` (SRS 5.5); пул соединений — отдельная секция
/// `database` ([`pool::PoolConfig`]). Все поля необязательны; неизвестное поле — ошибка разбора.
///
/// ```yaml
/// meta:
///   blob_tombstone_ttl: 24h
///   events:
///     lease_ttl: 30s
///   doctor:
///     timeout: 10s
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
    /// Подписчики журнала событий.
    pub events: EventsConfig,
    /// Пороги проверок СУБД `thepost doctor`.
    pub doctor: DoctorConfig,
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
        }
    }
}

/// Хранилища метаданных на PostgreSQL.
#[derive(Debug, Clone)]
pub struct PgMeta {
    pool: PgPool,
    cfg: Arc<MetaConfig>,
}

impl PgMeta {
    /// Хранилища над готовым пулом; схема должна быть применена.
    pub fn new(pool: PgPool, cfg: MetaConfig) -> Self {
        Self {
            pool,
            cfg: Arc::new(cfg),
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
  acquire_timeout: 5s
  idle_timeout: null
  max_lifetime: 1h30m
meta:
  blob_tombstone_ttl: 12h
  events:
    lease_ttl: 30s
  doctor:
    timeout: 250ms
"#,
        )
        .expect("yaml");
        assert_eq!(s.database.acquire_timeout, Duration::from_secs(5));
        assert_eq!(s.database.idle_timeout, None);
        assert_eq!(s.database.max_lifetime, Some(Duration::from_secs(5400)));
        assert_eq!(
            s.database.max_connections,
            PoolConfig::default().max_connections
        );
        assert_eq!(s.meta.blob_tombstone_ttl, Duration::from_secs(12 * 3600));
        assert_eq!(s.meta.events.lease_ttl, Duration::from_secs(30));
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
