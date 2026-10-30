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
pub mod pool;
pub mod schema;
pub mod shards;

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tp_contracts::Result;

/// Секция конфигурации контура метаданных (SRS 5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaConfig {
    /// Применять миграции схемы ядра при старте ([`PgMeta::open`]). Требует перезапуска.
    pub migrate_on_start: bool,
    /// Срок хранения надгробия BLOB после `finalize_delete`: загрузка, начатая до удаления объекта,
    /// получит `Bumped`. Больше максимальной длительности загрузки объекта (SRS 3.4).
    pub blob_tombstone_ttl: Duration,
    /// Число разделов журнала событий ядра; постоянно для инсталляции (смена — миграция журнала).
    pub event_partitions: u32,
}

impl Default for MetaConfig {
    fn default() -> Self {
        Self {
            migrate_on_start: true,
            blob_tombstone_ttl: Duration::from_secs(24 * 3600),
            event_partitions: 64,
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
