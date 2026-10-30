//! Разделы журнала между экземплярами: назначение по живым узлам, отдача лиз снятых разделов (META-16).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;
use tp_contracts::Result;
use tp_contracts::events::{
    EventLog, EventPos, EventSink, LoggedEvent, Partition, PartitionAssigner, partition_of,
};
use tp_contracts::membership::{Membership, NodeId};
use tp_contracts::meta::{AccountScope, MailboxStore, NewMailbox};
use tp_contracts::{AccountId, Error};
use tp_meta::events::{EventConsumer, EventsConfig};
use tp_meta::{MetaConfig, PgMeta};
use uuid::Uuid;

/// Разделов больше, чем у остальных тестов (64): разделы теста почти наверняка свои.
const PARTITIONS: u32 = 1 << 20;

async fn meta() -> Option<PgMeta> {
    let pool = common::pool().await?;
    Some(PgMeta::new(
        pool,
        MetaConfig {
            event_partitions: PARTITIONS,
            ..MetaConfig::default()
        },
    ))
}

/// Распределитель теста: разделы теста по кругу между живыми узлами теста (по возрастанию имени);
/// состав перечитывается из членства PostgreSQL вызовом [`TestAssigner::refresh`].
struct TestAssigner {
    meta: PgMeta,
    prefix: String,
    partitions: Vec<Partition>,
    members: watch::Sender<Vec<NodeId>>,
}

impl TestAssigner {
    async fn refresh(&self) {
        let live: Vec<_> = self
            .meta
            .live()
            .await
            .expect("live")
            .into_iter()
            .filter(|n| n.as_str().starts_with(&self.prefix))
            .collect();
        self.members.send_replace(live);
    }
}

impl PartitionAssigner for TestAssigner {
    fn assigned(&self, _subscriber: &str, node: &NodeId, _partitions: u32) -> Vec<Partition> {
        let members = self.members.borrow();
        let Some(index) = members.iter().position(|m| m == node) else {
            return Vec::new();
        };
        let mut out: Vec<_> = self
            .partitions
            .iter()
            .enumerate()
            .filter(|(i, _)| i % members.len() == index)
            .map(|(_, p)| *p)
            .collect();
        out.sort();
        out
    }

    fn members(&self) -> watch::Receiver<Vec<NodeId>> {
        self.members.subscribe()
    }
}

/// Подписчик экземпляра: запоминает обработанные события.
struct Recorder {
    name: String,
    seen: Mutex<Vec<LoggedEvent>>,
}

#[async_trait]
impl EventSink for Recorder {
    fn name(&self) -> &str {
        &self.name
    }

    async fn handle(&self, batch: &[LoggedEvent]) -> Result<()> {
        self.seen.lock().expect("lock").extend_from_slice(batch);
        Ok(())
    }
}

impl Recorder {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// События учётных записей теста: (раздел, позиция).
    fn seen(&self, accounts: &BTreeSet<AccountId>) -> Vec<(u32, u64)> {
        self.seen
            .lock()
            .expect("lock")
            .iter()
            .filter(|e| accounts.contains(&e.event.account))
            .map(|e| (e.partition.0, e.pos.0))
            .collect()
    }

    fn partitions(&self, accounts: &BTreeSet<AccountId>) -> BTreeSet<Partition> {
        self.seen(accounts)
            .into_iter()
            .map(|(p, _)| Partition(p))
            .collect()
    }
}

async fn add_mailbox(meta: &PgMeta, scope: AccountScope, name: &str) {
    meta.create(
        scope,
        NewMailbox {
            parent: None,
            name: name.into(),
            role: None,
        },
    )
    .await
    .expect("create");
}

/// Опрашивает обоих, пока оба не вернут 0.
async fn drain(a: (&EventConsumer, &Recorder), b: (&EventConsumer, &Recorder)) {
    for _ in 0..100 {
        let (ra, rb) = tokio::join!(a.0.poll(a.1), b.0.poll(b.1));
        if ra.expect("poll a") == 0 && rb.expect("poll b") == 0 {
            return;
        }
    }
    panic!("разделы не обработаны за 100 проходов");
}

#[tokio::test]
async fn partitions_follow_membership() {
    let Some(meta) = meta().await else {
        return;
    };
    // Четыре учётные записи в четырёх разных разделах вне диапазона других тестов.
    let mut by_partition = BTreeMap::new();
    while by_partition.len() < 4 {
        let scope = common::account(&meta).await;
        let p = partition_of(scope.account, PARTITIONS);
        if p.0 >= 64 {
            by_partition.entry(p).or_insert(scope);
        }
    }
    let parts: Vec<Partition> = by_partition.keys().copied().collect();
    let accounts: BTreeSet<AccountId> = by_partition.values().map(|s| s.account).collect();

    let prefix = format!("t{}", Uuid::now_v7().simple());
    let (a, b) = (
        NodeId::new(format!("{prefix}-a")).expect("a"),
        NodeId::new(format!("{prefix}-b")).expect("b"),
    );
    let ttl = Duration::from_secs(60);
    meta.heartbeat(&a, ttl).await.expect("heartbeat a");
    meta.heartbeat(&b, ttl).await.expect("heartbeat b");
    let assigner = Arc::new(TestAssigner {
        meta: meta.clone(),
        prefix: prefix.clone(),
        partitions: parts.clone(),
        members: watch::channel(Vec::new()).0,
    });
    assigner.refresh().await;
    assert_eq!(*assigner.members().borrow(), [a.clone(), b.clone()]);

    let cfg = EventsConfig {
        batch: 2,
        lease_ttl: Duration::from_secs(60),
        ..EventsConfig::default()
    };
    let shared = Arc::new(meta.clone());
    let consumer = |node: &NodeId| {
        EventConsumer::new(shared.clone(), shared.clone(), node.as_str(), cfg.clone())
            .with_assigner(assigner.clone(), node.clone())
    };
    let (ca, cb) = (consumer(&a), consumer(&b));
    let subscriber = format!("test-{}", Uuid::now_v7().simple());
    let (sa, sb) = (Recorder::new(&subscriber), Recorder::new(&subscriber));

    // Назначение пополам: каждый обрабатывает только свои разделы.
    let (mine_a, mine_b) = (ca.assigned(&subscriber), cb.assigned(&subscriber));
    assert_eq!(mine_a.len(), 2);
    assert_eq!(mine_b.len(), 2);
    drain((&ca, &sa), (&cb, &sb)).await;
    assert_eq!(sa.partitions(&accounts), mine_a.iter().copied().collect());
    assert_eq!(sb.partitions(&accounts), mine_b.iter().copied().collect());
    assert_eq!(ca.held(&subscriber), mine_a);
    assert_eq!(cb.held(&subscriber), mine_b);
    // Неназначенный раздел экземпляр не берёт и вызовом напрямую.
    assert_eq!(ca.poll_partition(&sa, mine_b[0]).await.expect("poll"), None);

    // Новые события во всех разделах, затем узел a выходит.
    for (i, scope) in by_partition.values().enumerate() {
        add_mailbox(&meta, *scope, &format!("after-{i}")).await;
    }
    meta.leave(&a).await.expect("leave");
    assigner.refresh().await;
    assert!(ca.assigned(&subscriber).is_empty(), "узел вне состава");
    // Следующий проход a отдаёт лизы снятых разделов, ничего не обрабатывая.
    assert_eq!(ca.poll(&sa).await.expect("poll a"), 0);
    assert!(ca.held(&subscriber).is_empty());
    assert_eq!(cb.assigned(&subscriber), parts);
    // b получает разделы a сразу, не дожидаясь срока лиз.
    drain((&ca, &sa), (&cb, &sb)).await;
    assert_eq!(cb.held(&subscriber), parts);

    // Ни одно событие не потеряно и не обработано дважды; позиции зафиксированы до конца.
    let mut handled: Vec<(u32, u64)> = sa.seen(&accounts);
    handled.extend(sb.seen(&accounts));
    let unique: BTreeSet<_> = handled.iter().copied().collect();
    assert_eq!(unique.len(), handled.len(), "событие обработано дважды");
    let mut expected = BTreeSet::new();
    for p in &parts {
        let events = meta.read(*p, EventPos(0), 10_000).await.expect("read");
        let last = events.last().map(|e| e.pos).expect("события раздела");
        assert_eq!(
            meta.position(&subscriber, *p).await.expect("position"),
            last
        );
        expected.extend(
            events
                .iter()
                .filter(|e| accounts.contains(&e.event.account))
                .map(|e| (p.0, e.pos.0)),
        );
    }
    assert_eq!(unique, expected);
    // Новые события после выхода a — у b.
    let after_leave: BTreeSet<_> = sb.partitions(&accounts);
    assert_eq!(after_leave, parts.iter().copied().collect());

    ca.release_all(&subscriber).await.expect("release a");
    cb.release_all(&subscriber).await.expect("release b");
    assert!(cb.held(&subscriber).is_empty());
    meta.leave(&b).await.expect("cleanup");
    assert!(matches!(
        meta.read(Partition(PARTITIONS), EventPos(0), 1).await,
        Err(Error::Invalid(_))
    ));
}
