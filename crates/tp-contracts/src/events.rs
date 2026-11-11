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
//!
//! Шарды (CTR-34, решение 2026-11-01-scale-shards-geo-admin, SRS 3.6): журнал событий ядра — на шарде ящика,
//! у каждого шарда свои разделы `0..N` и свои позиции подписчиков. Раздел инсталляции — [`ShardPartition`]
//! `{ shard, index }`: `index` = [`partition_of`]`(account, N)`, `shard` — шард учётной записи по справочнику шардов.
//! Журналы шардов — [`ShardEventLogs`]; назначение пар (шард, раздел) — [`PartitionAssigner::assigned_sharded`].
//! Один шард ([`ShardId::MAIN`]) — прежнее поведение: те же разделы, ключи назначения и имена лиз.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::watch;

use crate::error::Result;
use crate::ids::{AccountId, MailboxId, MessageId, ShardId, TenantId};
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

/// Номер раздела журнала шарда: `0..N` (`index` в [`ShardPartition`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Partition(pub u32);

impl fmt::Display for Partition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Раздел журнала инсталляции: раздел `index` журнала шарда `shard` (CTR-34).
/// Порядок — по шарду, затем по разделу; `Display` — `"{shard}/{index}"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ShardPartition {
    /// Шард, на котором хранится журнал.
    pub shard: ShardId,
    /// Раздел журнала шарда.
    pub index: Partition,
}

impl ShardPartition {
    /// Раздел `index` шарда `shard`.
    pub const fn new(shard: ShardId, index: Partition) -> Self {
        Self { shard, index }
    }

    /// Раздел учётной записи `account`, размещённой (по справочнику шардов) на шарде `shard`,
    /// при `partitions` разделах на шард: `index` = [`partition_of`]`(account, partitions)`.
    pub fn of(shard: ShardId, account: AccountId, partitions: u32) -> Self {
        Self::new(shard, partition_of(account, partitions))
    }

    /// Ключ подписчика этого шарда для назначения разделов и имени лизы — [`shard_subscriber`].
    pub fn subscriber_key<'a>(&self, subscriber: &'a str) -> Cow<'a, str> {
        shard_subscriber(subscriber, self.shard)
    }
}

impl fmt::Display for ShardPartition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.shard, self.index)
    }
}

/// Ключ подписчика `subscriber` на шарде `shard`: для [`ShardId::MAIN`] — само имя (прежние ключи назначения
/// и лизы `events/{subscriber}/{index}`), для остальных — `"{subscriber}@{shard}"`. Ключ передаётся в
/// [`PartitionAssigner::assigned`] и в имя лизы раздела: `events/{ключ}/{index}`.
pub fn shard_subscriber(subscriber: &str, shard: ShardId) -> Cow<'_, str> {
    if shard == ShardId::MAIN {
        Cow::Borrowed(subscriber)
    } else {
        Cow::Owned(format!("{subscriber}@{shard}"))
    }
}

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

/// Журналы событий шардов (CTR-34): у каждого шарда справочника — свой [`EventLog`] с одинаковым числом разделов
/// и своими позициями подписчиков. Реализация — `tp-meta` (пулы шардов по справочнику), in-memory — `tp-testkit`
/// (`MemShardedEventLog`); один журнал — [`SingleShard`].
pub trait ShardEventLogs: Send + Sync + 'static {
    /// Шарды справочника по возрастанию; всегда содержит [`ShardId::MAIN`]. Изменение справочника приходит
    /// сигналом — после него список перечитывают.
    fn shards(&self) -> Vec<ShardId>;

    /// Журнал шарда; шард вне справочника — `None`.
    fn log(&self, shard: ShardId) -> Option<Arc<dyn EventLog>>;

    /// Число разделов журнала шарда (одинаково для всех шардов, постоянно для инсталляции).
    fn partitions(&self) -> u32 {
        self.log(ShardId::MAIN).map_or(1, |l| l.partitions())
    }
}

/// Инсталляция с одним шардом: журнал [`ShardId::MAIN`] — прежнее поведение.
#[derive(Clone)]
pub struct SingleShard(pub Arc<dyn EventLog>);

impl fmt::Debug for SingleShard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SingleShard")
            .field("partitions", &self.0.partitions())
            .finish()
    }
}

impl ShardEventLogs for SingleShard {
    fn shards(&self) -> Vec<ShardId> {
        vec![ShardId::MAIN]
    }

    fn log(&self, shard: ShardId) -> Option<Arc<dyn EventLog>> {
        (shard == ShardId::MAIN).then(|| Arc::clone(&self.0))
    }

    fn partitions(&self) -> u32 {
        self.0.partitions()
    }
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

    /// Пары (шард, раздел) подписчика, назначенные узлу (CTR-34), по возрастанию; повторы в `shards` не важны.
    /// Раздел шарда назначается по ключу [`shard_subscriber`]: каждый раздел каждого шарда — ровно одному узлу
    /// состава, для [`ShardId::MAIN`] — как [`PartitionAssigner::assigned`]. Смена справочника шардов
    /// не переносит разделы прежних шардов.
    fn assigned_sharded(
        &self,
        subscriber: &str,
        node: &NodeId,
        shards: &[ShardId],
        partitions: u32,
    ) -> Vec<ShardPartition> {
        let mut shards = shards.to_vec();
        shards.sort_unstable();
        shards.dedup();
        shards
            .into_iter()
            .flat_map(|shard| {
                self.assigned(&shard_subscriber(subscriber, shard), node, partitions)
                    .into_iter()
                    .map(move |index| ShardPartition::new(shard, index))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_safe() {
        fn _logs(_: Arc<dyn ShardEventLogs>) {}
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
    fn sharded_assignment_covers_every_shard() {
        let members = nodes(&["n1", "n2", "n3"]);
        let a = Modulo(watch::Sender::new(members.clone()));
        let shards = [ShardId(2), ShardId::MAIN, ShardId(1), ShardId(2)];
        let mut all: Vec<ShardPartition> = members
            .iter()
            .flat_map(|n| {
                let mine = a.assigned_sharded("indexer", n, &shards, 5);
                assert!(mine.windows(2).all(|w| w[0] < w[1]), "по возрастанию");
                mine
            })
            .collect();
        all.sort();
        let expected: Vec<ShardPartition> = (0..3)
            .flat_map(|s| (0..5).map(move |p| ShardPartition::new(ShardId(s), Partition(p))))
            .collect();
        assert_eq!(all, expected);
        // Один шард — прежнее назначение.
        for n in &members {
            let main: Vec<Partition> = a
                .assigned_sharded("indexer", n, &[ShardId::MAIN], 5)
                .into_iter()
                .map(|p| p.index)
                .collect();
            assert_eq!(main, a.assigned("indexer", n, 5));
        }
        let outsider = NodeId::new("n9").expect("n9");
        assert!(
            a.assigned_sharded("indexer", &outsider, &shards, 5)
                .is_empty()
        );
    }

    #[test]
    fn shard_keys() {
        assert_eq!(shard_subscriber("indexer", ShardId::MAIN), "indexer");
        assert_eq!(shard_subscriber("indexer", ShardId(3)), "indexer@3");
        let a = AccountId::from_uuid(uuid::Uuid::from_u128(0x0123_4567_89ab_cdef));
        let p = ShardPartition::of(ShardId(3), a, 1024);
        assert_eq!(p.index, Partition(PINNED));
        assert_eq!(p.to_string(), format!("3/{PINNED}"));
        assert_eq!(p.subscriber_key("indexer"), "indexer@3");
        assert!(
            ShardPartition::new(ShardId(0), Partition(9))
                < ShardPartition::new(ShardId(1), Partition(0))
        );
        let json = serde_json::to_string(&p).expect("json");
        assert_eq!(json, format!("{{\"shard\":3,\"index\":{PINNED}}}"));
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
