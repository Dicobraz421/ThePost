//! Контур метаданных PostgreSQL: схема, миграции, реализации хранилищ (SRS 3.1, 3.4, 3.6).
//!
//! [`PgMeta`] — одна реализация трейтов `tp-contracts` над пулом PostgreSQL. Каждая операция —
//! короткая транзакция в контексте организации ([`schema::begin`]); к BLOB и сети внутри транзакций
//! реализация не обращается.

pub mod schema;

use std::sync::Arc;

use sqlx::PgPool;
use tp_contracts::Result;

/// Секция конфигурации контура метаданных (SRS 5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaConfig {
    /// Применять миграции схемы ядра при старте ([`PgMeta::open`]). Требует перезапуска.
    pub migrate_on_start: bool,
}

impl Default for MetaConfig {
    fn default() -> Self {
        Self {
            migrate_on_start: true,
        }
    }
}

/// Хранилища метаданных на PostgreSQL.
#[derive(Debug, Clone)]
pub struct PgMeta {
    pool: PgPool,
    // Читается реализациями хранилищ.
    #[allow(dead_code)]
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
