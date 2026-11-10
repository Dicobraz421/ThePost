//! Сигналы «изменилось X» между узлами (CONTRACTS.md#EventBus; SRS 2.8, решение 2026-11-01).
//!
//! Сигнал не несёт правды: получатель при запуске, после переподключения и по [`EventKind::Resync`] сверяется
//! с хранилищем (по `ModSeq`, позиции журнала, составу узлов, списку блокировок), далее работает по сигналам.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::abuse::BlockSubject;
use crate::error::Result;
use crate::events::Partition;
use crate::ids::{AccountId, MailboxId, ModSeq, TenantId};
use crate::meta::AccountScope;

/// Вид сигнала. Виды учётной записи (`account = Some`): `MailboxChanged`, `MailboxListChanged`; остальные —
/// системные (`account = None`, приходят в [`EventBus::subscribe_system`]). `Resync` вставляет в поток только шина.
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
    /// В раздел журнала событий записаны события (будит читателя раздела; `tenant` — `TenantId::DEFAULT`).
    LogAppended {
        /// Раздел журнала.
        partition: Partition,
    },
    /// Изменился состав кластера: вход, выход узла или его роли, адрес, площадка (`tenant` — `TenantId::DEFAULT`).
    /// Истечение записи узла сигналом не публикуется — это срок аренды.
    MembersChanged,
    /// Заблокирован источник или учётная запись организации `tenant` ([`crate::abuse::AbuseBlocks`]).
    Blocked {
        /// Субъект блокировки.
        subject: BlockSubject,
    },
    /// Сигналы подписки потеряны (переполнение буфера подписчика, переподключение слушателя): сверка с хранилищем.
    /// Шина вставляет его в поток подписки с `tenant`/`account` этой подписки (системная — `TenantId::DEFAULT`,
    /// `None`); `publish` с этим видом — `Invalid`.
    Resync,
}

impl EventKind {
    /// Системный вид: публикуется с `account = None`. `Resync` — не системный: его вставляет шина в любую подписку.
    pub fn is_system(&self) -> bool {
        !matches!(
            self,
            EventKind::MailboxChanged { .. } | EventKind::MailboxListChanged | EventKind::Resync
        )
    }
}

/// Сигнал. Без учётной записи (`account = None`) — системный.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Организация.
    pub tenant: TenantId,
    /// Учётная запись; `None` — системный сигнал ([`EventKind::is_system`]).
    pub account: Option<AccountId>,
    /// Вид.
    pub kind: EventKind,
}

/// Поток уведомлений подписки.
pub type EventStream = BoxStream<'static, Event>;

/// Шина сигналов: канал в памяти (один процесс, `tp-cluster`) или PostgreSQL LISTEN/NOTIFY (кластер, `tp-meta`).
///
/// Доставка at-most-once: сигнал публикуется только после фиксации изменения и может потеряться. У подписчика
/// ограниченный буфер: при переполнении сигналы отбрасываются, а подписчик получает [`EventKind::Resync`].
/// Поток завершается при потере связи — подписчик переподписывается и сверяется с хранилищем.
#[async_trait]
pub trait EventBus: Send + Sync + 'static {
    /// Публикует сигнал всем узлам. `EventKind::Resync` — `Invalid`.
    async fn publish(&self, event: Event) -> Result<()>;

    /// Сигналы одной учётной записи (виды учётной записи и `Resync`).
    async fn subscribe(&self, scope: AccountScope) -> Result<EventStream>;

    /// Системные сигналы (`account = None`) всех организаций и `Resync` этой подписки.
    async fn subscribe_system(&self) -> Result<EventStream>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_safe() {
        fn _bus(_: std::sync::Arc<dyn EventBus>) {}
    }

    #[test]
    fn system_kinds() {
        let mailbox = MailboxId::new();
        assert!(
            !EventKind::MailboxChanged {
                mailbox,
                modseq: ModSeq(1)
            }
            .is_system()
        );
        assert!(!EventKind::MailboxListChanged.is_system());
        assert!(!EventKind::Resync.is_system());
        for kind in [
            EventKind::QueueReady,
            EventKind::ConfigReloaded,
            EventKind::LogAppended {
                partition: Partition(3),
            },
            EventKind::MembersChanged,
            EventKind::Blocked {
                subject: BlockSubject::Account(AccountId::new()),
            },
        ] {
            assert!(kind.is_system(), "{kind:?}");
        }
    }
}
