//! Назначение разделов журнала событий узлам (CONTRACTS.md#EventLog; решение 2026-10-30, #45).
//!
//! Раздел `p` подписчика `s` достаётся узлу-владельцу ключа `s/p` по [`Rendezvous`] над живыми узлами
//! ([`Membership::live`]). Состав опрашивается с интервалом из [`PartitionAssignerConfig`]; новое значение
//! уходит в [`PartitionAssigner::members`], и подписчик пересчитывает свои разделы. Узел берёт лизы только
//! назначенных разделов и отдаёт лизы снятых ([`AssignmentChange`]); с одним узлом ему назначены все разделы.

use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tp_contracts::Result;
use tp_contracts::config::duration;
use tp_contracts::events::{Partition, PartitionAssigner};
use tp_contracts::membership::{Membership, NodeId};

use crate::hashing::Rendezvous;

/// Конфигурация назначения разделов. Незаданные поля — по умолчанию, неизвестные — ошибка разбора.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PartitionAssignerConfig {
    /// Интервал опроса живых узлов (`2s`, `500ms`; ноль считается за 1 мс). Смена состава замечается
    /// не позже чем через интервал; разумно — не больше периода продления записи узла (`ttl / 3`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub poll_interval: Duration,
}

impl Default for PartitionAssignerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
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
/// При ошибке опроса состав не меняется (последний известный): единственность обработки раздела
/// и в переходный период гарантирует лиза с номером захвата. Фоновый опрос останавливается вместе
/// с последней копией значения.
#[derive(Debug)]
pub struct RendezvousAssigner {
    members: watch::Sender<Vec<NodeId>>,
    poller: Option<JoinHandle<()>>,
}

impl RendezvousAssigner {
    /// Читает живые узлы и запускает их опрос с интервалом `config.poll_interval`.
    /// Ошибка первого чтения состава возвращается: без состава узлу нечего назначить.
    pub async fn start(
        membership: Arc<dyn Membership>,
        config: &PartitionAssignerConfig,
    ) -> Result<Self> {
        let members = watch::Sender::new(normalize(membership.live().await?));
        let period = config.poll_interval.max(Duration::from_millis(1));
        let poller = tokio::spawn(poll(membership, members.clone(), period));
        Ok(Self {
            members,
            poller: Some(poller),
        })
    }

    /// Неизменный состав без опроса (проверки, утилиты).
    pub fn fixed(members: Vec<NodeId>) -> Self {
        Self {
            members: watch::Sender::new(normalize(members)),
            poller: None,
        }
    }
}

impl Drop for RendezvousAssigner {
    fn drop(&mut self) {
        if let Some(poller) = self.poller.take() {
            poller.abort();
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

/// Опрос состава: новое значение отправляется, только если состав изменился.
async fn poll(
    membership: Arc<dyn Membership>,
    members: watch::Sender<Vec<NodeId>>,
    period: Duration,
) {
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Первый тик срабатывает сразу, а состав уже прочитан при запуске.
    tick.tick().await;
    loop {
        tick.tick().await;
        match membership.live().await {
            Ok(live) => {
                let live = normalize(live);
                members.send_if_modified(|current| {
                    if *current == live {
                        return false;
                    }
                    tracing::info!(nodes = live.len(), "состав кластера изменился");
                    *current = live;
                    true
                });
            }
            Err(e) => tracing::warn!(error = %e, "не удалось прочитать живые узлы, состав прежний"),
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
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tp_contracts::Error;

    use super::*;

    /// Членство в памяти для тестов полосы (до `MemMembership` из tp-testkit).
    #[derive(Default)]
    struct StubMembership {
        live: Mutex<Vec<NodeId>>,
        fail: Mutex<bool>,
    }

    impl StubMembership {
        fn set(&self, names: &[&str]) {
            *self.live.lock().expect("lock") = nodes(names);
        }

        fn fail(&self, fail: bool) {
            *self.fail.lock().expect("lock") = fail;
        }
    }

    #[async_trait]
    impl Membership for StubMembership {
        async fn heartbeat(&self, node: &NodeId, _ttl: Duration) -> Result<()> {
            let mut live = self.live.lock().expect("lock");
            if !live.contains(node) {
                live.push(node.clone());
            }
            Ok(())
        }

        async fn leave(&self, node: &NodeId) -> Result<()> {
            self.live.lock().expect("lock").retain(|n| n != node);
            Ok(())
        }

        async fn live(&self) -> Result<Vec<NodeId>> {
            if *self.fail.lock().expect("lock") {
                return Err(Error::Unavailable { retry_after: None });
            }
            Ok(self.live.lock().expect("lock").clone())
        }
    }

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
            serde_yaml_ng::from_str("poll_interval: 1m30s").expect("yaml");
        assert_eq!(c.poll_interval, Duration::from_secs(90));
        let c: PartitionAssignerConfig =
            serde_yaml_ng::from_str("poll_interval: 500ms").expect("yaml");
        assert_eq!(c.poll_interval, Duration::from_millis(500));
        assert_eq!(
            serde_yaml_ng::from_str::<PartitionAssignerConfig>("{}").expect("empty"),
            PartitionAssignerConfig::default()
        );
        for bad in [
            "poll_interval: 2000",
            "poll_interval: 2 s",
            "poll_interval_ms: 2000",
        ] {
            assert!(
                serde_yaml_ng::from_str::<PartitionAssignerConfig>(bad).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn config_interval_is_humantime() {
        assert_eq!(
            PartitionAssignerConfig::default().poll_interval,
            Duration::from_secs(2)
        );
        let schema = schemars::schema_for!(PartitionAssignerConfig)
            .as_value()
            .to_string();
        assert!(schema.contains("poll_interval"), "{schema}");
        assert!(!schema.contains("poll_interval_ms"), "{schema}");
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

    #[tokio::test]
    async fn polls_membership_and_notifies_on_change() {
        let m = Arc::new(StubMembership::default());
        m.set(&["n1"]);
        let config = PartitionAssignerConfig {
            poll_interval: Duration::from_millis(10),
        };
        let a = RendezvousAssigner::start(m.clone(), &config)
            .await
            .expect("start");
        let n1 = node("n1");
        // Один живой узел — ему назначены все разделы.
        assert_eq!(a.assigned("indexer", &n1, 16).len(), 16);
        let mut rx = a.members();
        let all = a.assigned("indexer", &n1, 16);

        m.heartbeat(&node("n2"), Duration::from_secs(30))
            .await
            .expect("heartbeat");
        assert_eq!(next(&mut rx).await, nodes(&["n1", "n2"]));
        let change = AssignmentChange::between(&all, &a.assigned("indexer", &n1, 16));
        assert!(change.taken.is_empty());
        assert!(
            !change.released.is_empty(),
            "n1 должен отдать часть разделов"
        );

        // Ошибка опроса не меняет состав; после восстановления изменения снова видны.
        m.fail(true);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!rx.has_changed().expect("open"));
        assert_eq!(*rx.borrow(), nodes(&["n1", "n2"]));
        m.fail(false);
        m.leave(&node("n2")).await.expect("leave");
        assert_eq!(next(&mut rx).await, nodes(&["n1"]));
        assert_eq!(a.assigned("indexer", &n1, 16), all);

        // Без изменений состава уведомлений нет.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!rx.has_changed().expect("open"));

        // Опрос останавливается вместе с назначением.
        drop(a);
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("опрос не остановился")
            .expect_err("канал закрыт");
    }

    #[tokio::test]
    async fn start_fails_without_membership() {
        let m = Arc::new(StubMembership::default());
        m.fail(true);
        let r = RendezvousAssigner::start(m, &PartitionAssignerConfig::default()).await;
        assert!(r.is_err());
    }
}
