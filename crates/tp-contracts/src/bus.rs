//! Сигналы «изменилось X» между узлами (CONTRACTS.md#EventBus; SRS 2.8, решение 2026-11-01).
//!
//! Сигнал не несёт правды: получатель при запуске, после переподключения и по [`EventKind::Resync`] сверяется
//! с хранилищем (по `ModSeq`, позиции журнала, составу узлов, списку блокировок, настройкам транспорта, сроку корзины),
//! далее работает по сигналам.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::abuse::BlockSubject;
use crate::dav::{CollectionId, CollectionKind};
use crate::error::Result;
use crate::events::Partition;
use crate::ids::{AccountId, MailboxId, ModSeq, TenantId};
use crate::meta::AccountScope;

/// Вид сигнала. Виды учётной записи (`account = Some`): `MailboxChanged`, `MailboxListChanged`, `DavChanged`,
/// `ScheduleInboxChanged`, `SieveChanged`; остальные —
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
    /// Изменилась коллекция календарей или контактов учётной записи: свойства, объекты, удаление
    /// ([`crate::dav::DavStore`]; JMAP push, кэши `tp-dav`). Сигнал — [`crate::dav::dav_changed`].
    DavChanged {
        /// Вид коллекции.
        kind: CollectionKind,
        /// Коллекция.
        collection: CollectionId,
        /// `ModSeq` изменения.
        modseq: ModSeq,
    },
    /// Изменились входящие планирования учётной записи (`DavStore::inbox_put`, `inbox_delete`; JMAP push
    /// `CalendarEventNotification`). Сигнал — [`crate::schedule::schedule_inbox_changed`].
    ScheduleInboxChanged {
        /// `ModSeq` изменения.
        modseq: ModSeq,
    },
    /// Изменились скрипты Sieve или автоответ учётной записи ([`crate::sieve::SieveStore`]; кэш программ `tp-sieve`,
    /// JMAP push). Сигнал — [`crate::sieve::sieve_changed`].
    SieveChanged,
    /// Очередь исходящей почты организации `tenant` пополнена или срок попытки перенесён (тема
    /// `<tenant>/queue/outbound`, CTR-73): после фиксации `QueueStore::enqueue`, `reschedule`, `postpone`. Будит
    /// smtp-out: он перечитывает ближайший срок (`QueueStore::next_due`). Сигнал — [`crate::queue::queue_ready`].
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
    /// Снята блокировка именно этого субъекта организации `tenant` ([`crate::abuse::AbuseBlocks::unblock`]):
    /// узел убирает `subject` из локального набора, не дожидаясь срока. Истечение срока сигналом не публикуется.
    Unblocked {
        /// Субъект снятой блокировки.
        subject: BlockSubject,
    },
    /// Изменились настройки транспорта ([`crate::transport::TransportStore`]): `tenant = None` — уровень кластера
    /// (принятые домены, доверенные источники; `Event::tenant` — `TenantId::DEFAULT`), иначе — организации.
    TransportChanged {
        /// Организация; `None` — уровень кластера.
        tenant: Option<TenantId>,
    },
    /// Изменилась корзина учётной записи `account` организации `tenant`: письма перенесены в корзину
    /// (`MailStore::delete_to_trash`) или очищены ([`crate::retention::TrashStore::purge`]). Будит таймер истечения
    /// корзины роли maintenance; сигнал — [`crate::retention::trash_changed`].
    TrashChanged {
        /// Учётная запись.
        account: AccountId,
    },
    /// Изменился справочник шардов метаданных: добавлен или выведен шард, изменились его адрес или реплика,
    /// учётная запись перенесена на другой шард (CTR-49, решение 47; `tenant` — `TenantId::DEFAULT`). Публикует тот,
    /// кто меняет справочник (модуль «Шардирование», администрирование), после фиксации; узлы сбрасывают кэш
    /// справочника (учётная запись → шард, список шардов, пулы).
    ShardsChanged,
    /// Вход учётной записи `account` организации `tenant` запрещён (CTR-64): после фиксации `enabled = false` или
    /// отключения ящика. Подписчики — протоколы с долгими сеансами (IMAP IDLE, JMAP push и EventSource) закрывают
    /// сеансы учётной записи; по `Resync` и при запуске сверяются с хранилищем. Сигнал —
    /// [`crate::meta::account_disabled`].
    AccountDisabled {
        /// Учётная запись.
        account: AccountId,
    },
    /// Отключённый ящик `account` организации `tenant` изменился (CTR-77): отключён, срок хранения изменён,
    /// восстановлен или подключён к другой учётной записи. Подписчик — воркер хранения отключённых ящиков роли
    /// maintenance: пересчитывает ближайший срок; по `Resync` и при запуске сверяется с хранилищем
    /// (`AccountStore::expired_accounts`). Сигнал — [`crate::meta::disabled_mailbox_changed`].
    DisabledMailboxChanged {
        /// Учётная запись.
        account: AccountId,
    },
    /// Изменилась книга GAL организации `tenant` (CTR-60): [`crate::dav::DavStore::sync_gal`] записал карточки или
    /// отображаемое имя. Подписчики — JMAP push и кэши `tp-dav` всех учётных записей организации; сверка — по
    /// [`crate::dav::Collection::state`] книги. Сигнал — [`crate::dav::gal_changed`].
    GalChanged {
        /// Книга GAL.
        collection: CollectionId,
        /// `ModSeq` книги после записи.
        modseq: ModSeq,
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
            EventKind::MailboxChanged { .. }
                | EventKind::MailboxListChanged
                | EventKind::DavChanged { .. }
                | EventKind::ScheduleInboxChanged { .. }
                | EventKind::SieveChanged
                | EventKind::Resync
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
        assert!(
            !EventKind::DavChanged {
                kind: CollectionKind::Calendar,
                collection: CollectionId::new(),
                modseq: ModSeq(2)
            }
            .is_system()
        );
        assert!(!EventKind::ScheduleInboxChanged { modseq: ModSeq(3) }.is_system());
        assert!(!EventKind::SieveChanged.is_system());
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
            EventKind::Unblocked {
                subject: BlockSubject::Account(AccountId::new()),
            },
            EventKind::TransportChanged { tenant: None },
            EventKind::TransportChanged {
                tenant: Some(TenantId::new()),
            },
            EventKind::TrashChanged {
                account: AccountId::new(),
            },
            EventKind::ShardsChanged,
            EventKind::AccountDisabled {
                account: AccountId::new(),
            },
            EventKind::DisabledMailboxChanged {
                account: AccountId::new(),
            },
            EventKind::GalChanged {
                collection: CollectionId::new(),
                modseq: ModSeq(4),
            },
        ] {
            assert!(kind.is_system(), "{kind:?}");
        }
    }
}
