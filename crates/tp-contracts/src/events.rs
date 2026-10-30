//! Журнал событий ядра (outbox) и его подписчики (CONTRACTS.md#Events, SRS 2.5).
//!
//! Событие записывается реализацией хранилища метаданных (`tp-meta`) в таблицу журнала
//! в той же транзакции, что и изменение: подписчик не пропустит изменение и не увидит неподтверждённое.
//! Поэтому записи в [`EventLog`] нет — только чтение и фиксация позиций подписчиков.
//!
//! Журнал разбит на разделы: раздел события = [`partition_of`]`(account, N)`, все события учётной записи
//! (а значит, и ящика) — в одном разделе, по возрастанию [`EventPos`]. Подписчик обрабатывает раздел
//! под лизой `events/{subscriber}/{partition}`: читает [`EventLog::read`], обрабатывает, фиксирует
//! [`EventLog::commit`]. Доставка не менее одного раза: после сбоя до `commit` события придут снова,
//! обработка должна быть идемпотентной.
//!
//! Между экземплярами разделы распределяет [`PartitionAssigner`] (решение 2026-10-30, #45): узел берёт лизу
//! только назначенного ему раздела и отдаёт лизы разделов, снятых с него, после фиксации текущей пачки.
//! Лиза с номером захвата остаётся гарантией, что раздел обрабатывает один экземпляр, в том числе пока узлы
//! видят разный состав (переходный период).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::watch;

use crate::error::Result;
use crate::ids::{AccountId, MailboxId, MessageId, TenantId};
use crate::membership::NodeId;

/// Вид события ядра.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoreEventKind {
    /// Письмо доставлено в папку.
    Delivered {
        /// Письмо.
        message: MessageId,
    },
    /// Письмо отправлено пользователем (submission, JMAP EmailSubmission).
    Submitted {
        /// Письмо (копия в «Отправленных», если сохранялась).
        message: Option<MessageId>,
    },
    /// Изменены флаги письма.
    FlagsChanged {
        /// Письмо.
        message: MessageId,
    },
    /// Письмо перенесено; `CoreEvent::mailbox` — папка назначения.
    Moved {
        /// Прежний идентификатор.
        from_message: MessageId,
        /// Прежняя папка.
        from_mailbox: MailboxId,
        /// Новый идентификатор.
        message: MessageId,
    },
    /// Письмо перенесено в корзину.
    Trashed {
        /// Письмо.
        message: MessageId,
    },
    /// Письмо удалено окончательно.
    Purged {
        /// Письмо.
        message: MessageId,
    },
    /// Создана папка (`CoreEvent::mailbox`).
    MailboxCreated,
    /// Удалена папка (`CoreEvent::mailbox`).
    MailboxDeleted,
}

/// Событие ядра.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreEvent {
    /// Организация.
    pub tenant: TenantId,
    /// Учётная запись (определяет раздел).
    pub account: AccountId,
    /// Папка, если событие к ней относится.
    pub mailbox: Option<MailboxId>,
    /// Вид.
    pub kind: CoreEventKind,
    /// Момент изменения (UTC).
    pub at: OffsetDateTime,
}

/// Номер раздела журнала: `0..N`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Partition(pub u32);

/// Позиция события в разделе: строго возрастает. `EventPos(0)` — до первого события.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct EventPos(pub u64);

/// Событие с позицией в журнале.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggedEvent {
    /// Раздел.
    pub partition: Partition,
    /// Позиция в разделе.
    pub pos: EventPos,
    /// Событие.
    pub event: CoreEvent,
}

/// Раздел учётной записи при `partitions` разделах (`partitions ≥ 1`).
///
/// Стабильная функция (FNV-1a по байтам UUID), одинаковая на всех узлах и версиях:
/// менять нельзя без миграции журнала.
pub fn partition_of(account: AccountId, partitions: u32) -> Partition {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in account.as_uuid().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Остаток меньше `partitions`, поэтому помещается в u32.
    Partition((h % u64::from(partitions.max(1))) as u32)
}

/// Журнал событий ядра: чтение разделов и позиции подписчиков. Реализация — `tp-meta`.
/// Читает события всех организаций (подписчики — системные роли и модули).
#[async_trait]
pub trait EventLog: Send + Sync + 'static {
    /// Число разделов (из конфигурации, постоянно для инсталляции).
    fn partitions(&self) -> u32;

    /// События раздела с позицией больше `after`, по возрастанию, не более `limit`.
    async fn read(
        &self,
        partition: Partition,
        after: EventPos,
        limit: usize,
    ) -> Result<Vec<LoggedEvent>>;

    /// Зафиксированная позиция подписчика в разделе; новый подписчик — `EventPos(0)`.
    async fn position(&self, subscriber: &str, partition: Partition) -> Result<EventPos>;

    /// Фиксирует обработку событий раздела до `pos` включительно. Позиция не уменьшается.
    async fn commit(&self, subscriber: &str, partition: Partition, pos: EventPos) -> Result<()>;
}

/// Подписчик журнала событий (индексатор, модули; регистрация — `Registry::add_event_sink`).
#[async_trait]
pub trait EventSink: Send + Sync + 'static {
    /// Имя подписчика — ключ его позиций; постоянно.
    fn name(&self) -> &str;

    /// Обрабатывает пачку событий одного раздела (по возрастанию позиции). Ошибка — пачка придёт снова.
    async fn handle(&self, batch: &[LoggedEvent]) -> Result<()>;
}

/// Назначение разделов журнала узлам по составу живых узлов ([`crate::membership::Membership::live`]).
/// Реализация — `tp-cluster` (rendezvous-хэширование ключа `<подписчик>/<раздел>`).
///
/// Для одного состава каждый раздел `0..partitions` назначен ровно одному узлу состава; назначение — функция
/// подписчика, раздела и состава, одинаковая на всех узлах. Узел вне состава не получает разделов, единственный
/// узел получает все. Смена состава переносит только разделы ушедших узлов и ≈ 1/N разделов на новый узел.
pub trait PartitionAssigner: Send + Sync + 'static {
    /// Разделы подписчика `subscriber`, назначенные узлу `node` при текущем составе, по возрастанию.
    fn assigned(&self, subscriber: &str, node: &NodeId, partitions: u32) -> Vec<Partition>;

    /// Текущий состав (по возрастанию); новое значение — повод пересчитать [`PartitionAssigner::assigned`].
    fn members(&self) -> watch::Receiver<Vec<NodeId>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _log(_: Arc<dyn EventLog>) {}
        fn _sink(_: Arc<dyn EventSink>) {}
        fn _assigner(_: Arc<dyn PartitionAssigner>) {}
    }

    /// Простейшее назначение для проверки контракта: раздел `p` — узлу `(p + len(подписчик)) mod N` состава.
    struct Modulo(watch::Sender<Vec<NodeId>>);

    impl PartitionAssigner for Modulo {
        fn assigned(&self, subscriber: &str, node: &NodeId, partitions: u32) -> Vec<Partition> {
            let members = self.0.borrow();
            let Some(index) = members.iter().position(|m| m == node) else {
                return Vec::new();
            };
            (0..partitions)
                .filter(|p| (*p as usize + subscriber.len()) % members.len() == index)
                .map(Partition)
                .collect()
        }

        fn members(&self) -> watch::Receiver<Vec<NodeId>> {
            self.0.subscribe()
        }
    }

    fn nodes(names: &[&str]) -> Vec<NodeId> {
        names
            .iter()
            .map(|n| NodeId::new(*n).expect("node"))
            .collect()
    }

    /// Инвариант контракта: разделы узлов состава не пересекаются и покрывают `0..partitions`.
    fn check_cover(a: &dyn PartitionAssigner, partitions: u32) {
        let members = a.members().borrow().clone();
        let mut all: Vec<Partition> = members
            .iter()
            .flat_map(|n| a.assigned("indexer", n, partitions))
            .collect();
        all.sort();
        assert_eq!(all, (0..partitions).map(Partition).collect::<Vec<_>>());
    }

    #[test]
    fn membership_change_changes_assignment() {
        let a = Modulo(watch::Sender::new(nodes(&["n1"])));
        let mut rx = a.members();
        let n1 = NodeId::new("n1").expect("n1");
        let n2 = NodeId::new("n2").expect("n2");
        // Один узел — все разделы; узел вне состава — ни одного.
        assert_eq!(a.assigned("indexer", &n1, 4).len(), 4);
        assert!(a.assigned("indexer", &n2, 4).is_empty());
        check_cover(&a, 4);

        a.0.send_replace(nodes(&["n1", "n2"]));
        assert!(rx.has_changed().expect("open"));
        assert_eq!(rx.borrow_and_update().len(), 2);
        let (p1, p2) = (a.assigned("indexer", &n1, 4), a.assigned("indexer", &n2, 4));
        assert_eq!((p1.len(), p2.len()), (2, 2));
        assert!(p1.iter().all(|p| !p2.contains(p)));
        check_cover(&a, 4);

        a.0.send_replace(nodes(&["n2"]));
        assert!(a.assigned("indexer", &n1, 4).is_empty());
        assert_eq!(a.assigned("indexer", &n2, 4).len(), 4);
        check_cover(&a, 4);
    }

    #[test]
    fn partition_stable_and_in_range() {
        let a = AccountId::from_uuid(uuid::Uuid::from_u128(0x0123_4567_89ab_cdef));
        assert_eq!(partition_of(a, 16), partition_of(a, 16));
        assert_eq!(partition_of(a, 1), Partition(0));
        assert_eq!(partition_of(a, 0), Partition(0));
        for _ in 0..100 {
            assert!(partition_of(AccountId::new(), 7).0 < 7);
        }
        // Значение зафиксировано: изменение функции ломает существующие журналы.
        assert_eq!(partition_of(a, 1024), Partition(PINNED));
    }

    const PINNED: u32 = 645;
}
