//! Шина уведомлений между узлами (CONTRACTS.md#EventBus, SRS 2.1).

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ids::{AccountId, MailboxId, ModSeq, TenantId};
use crate::meta::AccountScope;

/// Вид уведомления.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    /// Изменилось содержимое папки (JMAP push, IMAP IDLE).
    MailboxChanged {
        /// Папка.
        mailbox: MailboxId,
        /// `ModSeq` изменения.
        modseq: ModSeq,
    },
    /// Изменился список папок учётной записи.
    MailboxListChanged,
    /// В очереди исходящей почты появились записи (будит smtp-out).
    QueueReady,
    /// Конфигурация перечитана.
    ConfigReloaded,
}

/// Уведомление. Без учётной записи (`account = None`) — системное.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Организация.
    pub tenant: TenantId,
    /// Учётная запись; `None` — системное уведомление (`QueueReady`, `ConfigReloaded`).
    pub account: Option<AccountId>,
    /// Вид.
    pub kind: EventKind,
}

/// Поток уведомлений подписки.
pub type EventStream = BoxStream<'static, Event>;

/// Шина уведомлений: в процессе (SMB) или Valkey (Enterprise), реализации — `tp-cluster`.
///
/// Доставка at-most-once: уведомление может потеряться (переподключение, перегрузка подписчика),
/// поэтому подписчик после переподключения сверяется с хранилищем по `ModSeq`. Поток завершается
/// при потере связи — подписчик переподписывается.
#[async_trait]
pub trait EventBus: Send + Sync + 'static {
    /// Публикует уведомление всем узлам.
    async fn publish(&self, event: Event) -> Result<()>;

    /// Уведомления одной учётной записи.
    async fn subscribe(&self, scope: AccountScope) -> Result<EventStream>;

    /// Системные уведомления (`account = None`) всех организаций.
    async fn subscribe_system(&self) -> Result<EventStream>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_safe() {
        fn _bus(_: std::sync::Arc<dyn EventBus>) {}
    }
}
