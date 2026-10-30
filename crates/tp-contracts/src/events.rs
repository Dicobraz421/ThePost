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

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::Result;
use crate::ids::{AccountId, MailboxId, MessageId, TenantId};

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _log(_: Arc<dyn EventLog>) {}
        fn _sink(_: Arc<dyn EventSink>) {}
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
