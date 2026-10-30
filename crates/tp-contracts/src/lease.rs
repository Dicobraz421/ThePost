//! Лизы — распределённая аренда с истечением (CONTRACTS.md#Lease).

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::Result;

/// Полученная лиза.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Имя ресурса (`maintenance-leader`, `indexer/{tenant}/{account}`, `events/{subscriber}/{partition}`, ...).
    pub name: String,
    /// Держатель (идентификатор узла и процесса).
    pub holder: String,
    /// Номер захвата: растёт при каждом новом захвате ресурса (fencing token).
    /// Записи, защищённые лизой, проверяют, что номер не устарел.
    pub fencing: u64,
    /// Момент истечения (UTC).
    pub expires_at: OffsetDateTime,
}

/// Хранилище лиз. Реализация — PostgreSQL (`tp-meta`); in-memory — `tp-testkit`.
/// Применение: лидер maintenance, индексатор ящика, записи очереди, разделы журнала событий.
#[async_trait]
pub trait LeaseStore: Send + Sync + 'static {
    /// Захватывает свободную или истёкшую лизу на `ttl`; `None` — занята другим держателем.
    /// Повторный захват тем же держателем продлевает лизу.
    async fn try_acquire(&self, name: &str, holder: &str, ttl: Duration) -> Result<Option<Lease>>;

    /// Продлевает лизу на `ttl` от текущего момента; `None` — лиза потеряна (истекла и захвачена другим).
    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<Option<Lease>>;

    /// Освобождает лизу; уже потерянная — не ошибка.
    async fn release(&self, lease: &Lease) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_safe() {
        fn _lease(_: std::sync::Arc<dyn LeaseStore>) {}
    }
}
