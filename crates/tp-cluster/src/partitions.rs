//! Назначение разделов журнала событий узлам (CONTRACTS.md#EventLog; решения 2026-10-30, #45, и 2026-11-01).
//!
//! Раздел `p` подписчика `s` достаётся узлу-владельцу ключа `s/p` по [`Rendezvous`] над живыми узлами
//! ([`Membership::live`]). Состав перечитывается по сигналу [`EventKind::MembersChanged`] (вход, выход узла),
//! по [`EventKind::Resync`] и после переподписки на сигналы (SRS 2.8, правило 3); опроса нет. Истечение записи
//! узла сигналом не публикуется — его ловит единственный таймер, таймер аренды: узел, живой при чтении состава,
//! истекает не позже чем через срок записи `node_ttl`, поэтому состав перечитывается через `node_ttl` после
//! последнего чтения. Новое значение уходит в [`PartitionAssigner::members`], и подписчик пересчитывает свои
//! разделы. Узел берёт лизы только назначенных разделов и отдаёт лизы снятых ([`AssignmentChange`]); с одним
//! узлом ему назначены все разделы.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tp_contracts::Result;
use tp_contracts::bus::{EventBus, EventKind, EventStream};
use tp_contracts::config::duration;
use tp_contracts::events::{Partition, PartitionAssigner};
use tp_contracts::membership::{Membership, NodeId};

use crate::hashing::Rendezvous;

/// Конфигурация назначения разделов. Незаданные поля — по умолчанию, неизвестные — ошибка разбора.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PartitionAssignerConfig {
    /// Срок записи узла в членстве (тот же, с которым узлы продлевают запись, `meta.events.node_ttl`):
    /// через столько после чтения состава он перечитывается, чтобы заметить истёкшие записи (ноль — 1 мс).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub node_ttl: Duration,
    /// Пауза перед повтором чтения состава или подписки на сигналы после временной ошибки (ноль — 1 мс).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry_delay: Duration,
}

impl Default for PartitionAssignerConfig {
    fn default() -> Self {
        Self {
            node_ttl: Duration::from_secs(15),
            retry_delay: Duration::from_secs(1),
        }
    }
}

/// Ключ хэширования раздела: `<подписчик>/<раздел>` (номер раздела — десятичный).
fn partition_key(subscriber: &str, partition: Partition) -> String {
    format!("{subscriber}/{}", partition.0)
}

/// Разделы `0..partitions` подписчика, назначенные узлу `node` при составе `members`, по возрастанию.
fn assign(members: &[NodeId], subscriber: &str, node: &NodeId, partitions: u32) -> Vec<Partition> {
    if !members.contains(node) {
        return Vec::new();
    }
    let ring = Rendezvous::new(members.iter().map(NodeId::as_str));
    (0..partitions)
        .map(Partition)
        .filter(|&p| ring.owner_of(partition_key(subscriber, p).as_bytes()) == Some(node.as_str()))
        .collect()
}

/// Нормализованный состав: по возрастанию, без повторов.
fn normalize(mut members: Vec<NodeId>) -> Vec<NodeId> {
    members.sort();
    members.dedup();
    members
}

/// Назначение разделов rendezvous-хэшированием по живым узлам.
///
/// При ошибке чтения состава он не меняется (последний известный) до успешного повтора: единственность
/// обработки раздела и в переходный период гарантирует лиза с номером захвата. Фоновое слежение за составом
/// останавливается вместе с последней копией значения.
#[derive(Debug)]
pub struct RendezvousAssigner {
    members: watch::Sender<Vec<NodeId>>,
    follower: Option<JoinHandle<()>>,
}

impl RendezvousAssigner {
    /// Подписывается на системные сигналы `bus`, читает живые узлы и следит за составом (см. модуль).
    /// Ошибка подписки или первого чтения состава возвращается: без состава узлу нечего назначить.
    pub async fn start(
        membership: Arc<dyn Membership>,
        bus: Arc<dyn EventBus>,
        config: &PartitionAssignerConfig,
    ) -> Result<Self> {
        // Сначала подписка, затем чтение: изменение между ними придёт сигналом.
        let signals = bus.subscribe_system().await?;
        let members = watch::Sender::new(normalize(membership.live().await?));
        let follower = Follower {
            membership,
            bus,
            members: members.clone(),
            node_ttl: config.node_ttl.max(Duration::from_millis(1)),
            retry_delay: config.retry_delay.max(Duration::from_millis(1)),
        };
        let follower = tokio::spawn(follower.run(signals));
        Ok(Self {
            members,
            follower: Some(follower),
        })
    }

    /// Неизменный состав без слежения (проверки, утилиты).
    pub fn fixed(members: Vec<NodeId>) -> Self {
        Self {
            members: watch::Sender::new(normalize(members)),
            follower: None,
        }
    }
}

impl Drop for RendezvousAssigner {
    fn drop(&mut self) {
        if let Some(follower) = self.follower.take() {
            follower.abort();
        }
    }
}

impl PartitionAssigner for RendezvousAssigner {
    fn assigned(&self, subscriber: &str, node: &NodeId, partitions: u32) -> Vec<Partition> {
        assign(&self.members.borrow(), subscriber, node, partitions)
    }

    fn members(&self) -> watch::Receiver<Vec<NodeId>> {
        self.members.subscribe()
    }
}

/// Фоновое слежение за составом: сигналы, переподписка и таймер аренды.
struct Follower {
    membership: Arc<dyn Membership>,
    bus: Arc<dyn EventBus>,
    members: watch::Sender<Vec<NodeId>>,
    node_ttl: Duration,
    retry_delay: Duration,
}

impl Follower {
    async fn run(self, signals: EventStream) {
        let mut signals = Some(signals);
        // Состав прочитан при запуске: следующее чтение — по сигналу или по истечении аренды.
        let mut wake = Instant::now() + self.node_ttl;
        loop {
            if signals.is_none() {
                match self.bus.subscribe_system().await {
                    Ok(stream) => {
                        signals = Some(stream);
                        // После переподписки — сверка с хранилищем: сигналы могли потеряться.
                        wake = self.refresh().await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "нет подписки на сигналы состава, повтор");
                        tokio::time::sleep(self.retry_delay).await;
                        continue;
                    }
                }
            }
            let Some(stream) = signals.as_mut() else {
                continue;
            };
            tokio::select! {
                event = stream.next() => match event {
                    Some(e) if matches!(e.kind, EventKind::MembersChanged | EventKind::Resync) => {
                        wake = self.refresh().await;
                    }
                    Some(_) => {}
                    None => {
                        tracing::debug!("поток сигналов состава завершён, переподписка");
                        signals = None;
                    }
                },
                () = tokio::time::sleep_until(wake) => wake = self.refresh().await,
            }
        }
    }

    /// Перечитывает состав; новое значение отправляется, только если состав изменился. Возвращает срок
    /// следующего чтения: истечение аренды после успеха или повтор после ошибки.
    async fn refresh(&self) -> Instant {
        match self.membership.live().await {
            Ok(live) => {
                let live = normalize(live);
                self.members.send_if_modified(|current| {
                    if *current == live {
                        return false;
                    }
                    tracing::info!(nodes = live.len(), "состав кластера изменился");
                    *current = live;
                    true
                });
                Instant::now() + self.node_ttl
            }
            Err(e) => {
                tracing::warn!(error = %e, "не удалось прочитать живые узлы, состав прежний");
                Instant::now() + self.retry_delay
            }
        }
    }
}

/// Изменение назначенных узлу разделов между двумя составами.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssignmentChange {
    /// Назначены заново: взять лизы.
    pub taken: Vec<Partition>,
    /// Сняты: зафиксировать текущую пачку и отдать лизы.
    pub released: Vec<Partition>,
}

impl AssignmentChange {
    /// Разница между прежним и новым назначением (оба — по возрастанию или в любом порядке).
    pub fn between(before: &[Partition], after: &[Partition]) -> Self {
        Self {
            taken: after
                .iter()
                .filter(|p| !before.contains(p))
                .copied()
                .collect(),
            released: before
                .iter()
                .filter(|p| !after.contains(p))
                .copied()
                .collect(),
        }
    }

    /// Назначение не изменилось.
    pub fn is_empty(&self) -> bool {
        self.taken.is_empty() && self.released.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use tp_contracts::Error;
    use tp_contracts::TenantId;
    use tp_contracts::bus::Event;
    use tp_contracts::membership::NodeInfo;
    use tp_testkit::{ManualClock, MemEventBus, MemMembership};

    use super::*;

    fn node(name: &str) -> NodeId {
        NodeId::new(name).expect("node")
    }

    fn nodes(names: &[&str]) -> Vec<NodeId> {
        names.iter().map(|n| node(n)).collect()
    }

    /// Владелец каждого раздела `0..partitions` при составе назначения.
    fn owners(a: &dyn PartitionAssigner, subscriber: &str, partitions: u32) -> Vec<NodeId> {
        let members = a.members().borrow().clone();
        let mut owner = vec![None; partitions as usize];
        for n in &members {
            for p in a.assigned(subscriber, n, partitions) {
                assert!(
                    owner[p.0 as usize].is_none(),
                    "раздел {} назначен дважды",
                    p.0
                );
                owner[p.0 as usize] = Some(n.clone());
            }
        }
        owner
            .into_iter()
            .enumerate()
            .map(|(p, o)| o.unwrap_or_else(|| panic!("раздел {p} никому не назначен")))
            .collect()
    }

    #[test]
    fn single_node_gets_all_outsider_none() {
        let a = RendezvousAssigner::fixed(nodes(&["solo"]));
        let all: Vec<Partition> = (0..64).map(Partition).collect();
        assert_eq!(a.assigned("indexer", &node("solo"), 64), all);
        assert!(a.assigned("indexer", &node("other"), 64).is_empty());
        assert!(a.assigned("indexer", &node("solo"), 0).is_empty());
        assert!(
            RendezvousAssigner::fixed(Vec::new())
                .assigned("indexer", &node("solo"), 8)
                .is_empty()
        );
    }

    #[test]
    fn assignment_is_sorted_cover_and_order_independent() {
        let a = RendezvousAssigner::fixed(nodes(&["n3", "n1", "n2", "n1"]));
        let b = RendezvousAssigner::fixed(nodes(&["n1", "n2", "n3"]));
        assert_eq!(*a.members().borrow(), nodes(&["n1", "n2", "n3"]));
        assert_eq!(owners(&a, "indexer", 256), owners(&b, "indexer", 256));
        for n in nodes(&["n1", "n2", "n3"]) {
            let mine = a.assigned("indexer", &n, 256);
            assert!(mine.windows(2).all(|w| w[0] < w[1]));
            assert!((40..130).contains(&mine.len()), "{n}: {}", mine.len());
        }
        // Ключ включает подписчика: разные подписчики распределены по-разному.
        assert_ne!(owners(&a, "indexer", 256), owners(&a, "mod_audit", 256));
    }

    #[test]
    fn parses_yaml_section() {
        let c: PartitionAssignerConfig =
            serde_yaml_ng::from_str("node_ttl: 1m30s\nretry_delay: 500ms").expect("yaml");
        assert_eq!(c.node_ttl, Duration::from_secs(90));
        assert_eq!(c.retry_delay, Duration::from_millis(500));
        assert_eq!(
            serde_yaml_ng::from_str::<PartitionAssignerConfig>("{}").expect("empty"),
            PartitionAssignerConfig::default()
        );
        // Периода опроса больше нет (CLU-10).
        for bad in ["node_ttl: 2000", "retry_delay: 2 s", "poll_interval: 2s"] {
            assert!(
                serde_yaml_ng::from_str::<PartitionAssignerConfig>(bad).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn config_durations_are_humantime() {
        let d = PartitionAssignerConfig::default();
        assert_eq!(
            (d.node_ttl, d.retry_delay),
            (Duration::from_secs(15), Duration::from_secs(1))
        );
        let schema = schemars::schema_for!(PartitionAssignerConfig)
            .as_value()
            .to_string();
        assert!(
            schema.contains("node_ttl") && schema.contains("retry_delay"),
            "{schema}"
        );
        assert!(!schema.contains("poll_interval"), "{schema}");
        assert!(schema.contains("\"type\":\"string\""), "{schema}");
    }

    #[test]
    fn key_is_pinned() {
        // Ключ `<подписчик>/<раздел>` одинаков на всех узлах и версиях.
        assert_eq!(partition_key("indexer", Partition(17)), "indexer/17");
    }

    #[test]
    fn adding_node_moves_about_one_nth_only_to_it() {
        const P: u32 = 2_000;
        let before = owners(
            &RendezvousAssigner::fixed(nodes(&["n1", "n2", "n3", "n4"])),
            "indexer",
            P,
        );
        let after = owners(
            &RendezvousAssigner::fixed(nodes(&["n1", "n2", "n3", "n4", "n5"])),
            "indexer",
            P,
        );
        let mut moved = 0;
        for (old, new) in before.iter().zip(&after) {
            if old != new {
                assert_eq!(new.as_str(), "n5", "раздел переехал не на новый узел");
                moved += 1;
            }
        }
        // Новый узел забирает около 1/5 разделов (400).
        assert!((300..500).contains(&moved), "moved = {moved}");
    }

    #[test]
    fn removing_node_moves_only_its_partitions() {
        const P: u32 = 2_000;
        let before = owners(
            &RendezvousAssigner::fixed(nodes(&["n1", "n2", "n3", "n4", "n5"])),
            "indexer",
            P,
        );
        let after = owners(
            &RendezvousAssigner::fixed(nodes(&["n1", "n2", "n4", "n5"])),
            "indexer",
            P,
        );
        let mut moved = 0;
        for (old, new) in before.iter().zip(&after) {
            if old.as_str() == "n3" {
                assert_ne!(new.as_str(), "n3");
                moved += 1;
            } else {
                assert_eq!(new, old, "раздел оставшегося узла переехал");
            }
        }
        assert!(moved > 0);
    }

    #[test]
    fn change_lists_taken_and_released() {
        let before = [Partition(0), Partition(2), Partition(3)];
        let after = [Partition(2), Partition(3), Partition(5)];
        let c = AssignmentChange::between(&before, &after);
        assert_eq!(c.taken, vec![Partition(5)]);
        assert_eq!(c.released, vec![Partition(0)]);
        assert!(!c.is_empty());
        assert!(AssignmentChange::between(&after, &after).is_empty());
    }

    async fn next(rx: &mut watch::Receiver<Vec<NodeId>>) -> Vec<NodeId> {
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("состав не обновился")
            .expect("канал открыт");
        rx.borrow_and_update().clone()
    }

    /// Срок записей узлов и аренды в тестах: дольше теста, чтобы изменения приходили только сигналом.
    const LONG: Duration = Duration::from_secs(3600);

    fn config(node_ttl: Duration) -> PartitionAssignerConfig {
        PartitionAssignerConfig {
            node_ttl,
            retry_delay: Duration::from_millis(10),
        }
    }

    async fn join(m: &MemMembership, name: &str, ttl: Duration) {
        m.heartbeat(&NodeInfo::new(node(name)), ttl)
            .await
            .expect("heartbeat");
    }

    async fn start(m: &MemMembership, bus: &MemEventBus, node_ttl: Duration) -> RendezvousAssigner {
        RendezvousAssigner::start(
            Arc::new(m.clone()),
            Arc::new(bus.clone()),
            &config(node_ttl),
        )
        .await
        .expect("start")
    }

    #[tokio::test]
    async fn join_and_leave_reassign_by_signal() {
        let bus = MemEventBus::default();
        let m = MemMembership::default().with_bus(Arc::new(bus.clone()));
        join(&m, "n1", LONG).await;
        // Аренда — час: изменения видны только по сигналу, без ожидания периода.
        let a = start(&m, &bus, LONG).await;
        let n1 = node("n1");
        // Один живой узел — ему назначены все разделы.
        let all = a.assigned("indexer", &n1, 16);
        assert_eq!(all.len(), 16);
        let mut rx = a.members();

        join(&m, "n2", LONG).await;
        assert_eq!(next(&mut rx).await, nodes(&["n1", "n2"]));
        let change = AssignmentChange::between(&all, &a.assigned("indexer", &n1, 16));
        assert!(change.taken.is_empty());
        assert!(
            !change.released.is_empty(),
            "n1 должен отдать часть разделов"
        );

        m.leave(&node("n2")).await.expect("leave");
        assert_eq!(next(&mut rx).await, nodes(&["n1"]));
        assert_eq!(a.assigned("indexer", &n1, 16), all);

        // Продление без изменений записи сигнала не даёт — и уведомления о составе тоже.
        join(&m, "n1", LONG).await;
        bus.publish(Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind: EventKind::MembersChanged,
        })
        .await
        .expect("publish");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!rx.has_changed().expect("open"));

        // Слежение останавливается вместе с назначением.
        drop(a);
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("слежение не остановилось")
            .expect_err("канал закрыт");
    }

    #[tokio::test]
    async fn expiry_is_caught_by_lease_timer() {
        let bus = MemEventBus::default();
        let clock = ManualClock::default();
        let m = MemMembership::new(clock.shared()).with_bus(Arc::new(bus.clone()));
        join(&m, "n1", LONG).await;
        join(&m, "n2", Duration::from_secs(15)).await;
        let a = start(&m, &bus, Duration::from_millis(20)).await;
        let mut rx = a.members();
        assert_eq!(*rx.borrow(), nodes(&["n1", "n2"]));
        // Запись n2 истекла: сигнала нет, состав перечитывается по таймеру аренды.
        clock.advance(Duration::from_secs(16));
        assert_eq!(next(&mut rx).await, nodes(&["n1"]));
    }

    #[tokio::test]
    async fn resync_and_resubscribe_reread_membership() {
        // Без шины у членства: изменения состава приходят только сверкой.
        let bus = MemEventBus::new(1);
        let m = MemMembership::default();
        join(&m, "n1", LONG).await;
        let a = start(&m, &bus, LONG).await;
        let mut rx = a.members();

        // Переполнение буфера подписчика — Resync — сверка.
        join(&m, "n2", LONG).await;
        for _ in 0..3 {
            bus.publish(Event {
                tenant: TenantId::DEFAULT,
                account: None,
                kind: EventKind::QueueReady,
            })
            .await
            .expect("publish");
        }
        assert_eq!(next(&mut rx).await, nodes(&["n1", "n2"]));

        // Потеря связи — переподписка — сверка.
        m.leave(&node("n2")).await.expect("leave");
        bus.disconnect();
        assert_eq!(next(&mut rx).await, nodes(&["n1"]));
        assert_eq!(bus.subscribers(), 1);
    }

    #[tokio::test]
    async fn read_error_keeps_members_and_retries() {
        let bus = MemEventBus::default();
        let m = MemMembership::default().with_bus(Arc::new(bus.clone()));
        join(&m, "n1", LONG).await;
        let a = start(&m, &bus, LONG).await;
        let mut rx = a.members();
        // Сигнал уже в буфере; чтение по нему завершится ошибкой, состав прежний до повтора.
        join(&m, "n2", LONG).await;
        m.fail_next(Error::UNAVAILABLE);
        assert_eq!(next(&mut rx).await, nodes(&["n1", "n2"]));
    }

    #[tokio::test]
    async fn start_fails_without_membership() {
        let bus = MemEventBus::default();
        let m = MemMembership::default();
        m.fail_next(Error::UNAVAILABLE);
        let r = RendezvousAssigner::start(
            Arc::new(m),
            Arc::new(bus),
            &PartitionAssignerConfig::default(),
        )
        .await;
        assert!(r.is_err());
    }
}
