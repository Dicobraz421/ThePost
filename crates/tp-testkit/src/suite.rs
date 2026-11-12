//! Общие наборы тестов контрактов: in-memory реализации testkit и настоящие реализации полос
//! (`tp-meta` — `Membership`, `AbuseBlocks`, сигналы `EventBus`, `AclStore`, `AppPasswordStore`, `DeliveryLog`, `GroupStore`, сжатие `BlobIndex`,
//! «очистить корзину» `TrashStore::purge` с настройкой `trash.purge`, `TransportStore` и его таблица переписывания,
//! перенос получателей `QueueStore::postpone`, статусы `Outbound::submit` (`tp-smtp` — `Queue`, `Delivery`),
//! `tp-platform` — `SecretStore`, `tp-cluster` — `Rpc`) проходят одни и те же проверки.
//!
//! Время в наборах идёт через [`Wait`]: для testkit — [`ManualClock`] (сдвиг без ожидания), для PostgreSQL
//! и OpenBao — [`RealTime`] (сон), тогда сроки выбирают с запасом на задержки.
//!
//! ```
//! # tokio_test_block_on(async {
//! use std::time::Duration;
//! use tp_testkit::{ManualClock, MemMembership, suite};
//!
//! let clock = ManualClock::default();
//! suite::membership(&MemMembership::new(clock.shared()), Duration::from_secs(30), &clock).await;
//! # });
//! # fn tokio_test_block_on(f: impl std::future::Future<Output = ()>) {
//! #     tokio::runtime::Builder::new_current_thread().enable_all().build().expect("rt").block_on(f)
//! # }
//! ```

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use time::OffsetDateTime;
use tp_contracts::abuse::{AbuseBlock, AbuseBlocks, BlockSubject};
use tp_contracts::acl::{ANYONE, AclEntry, AclStore, SendDelegation, SharedMailbox};
use tp_contracts::addressbook::{
    ContactStore, DynamicGroup, DynamicGroupStore, MailContact, MemberFilter, Members,
    OrgAttributes, OrgMember, OrgStore, OrgUnit, unit_path,
};
use tp_contracts::admin_token::{LocalAdminToken, LocalAdminTokens};
use tp_contracts::app_password::{AppPassword, AppPasswordStore};
use tp_contracts::audit::{
    AuditAction, AuditActorKind, AuditCursor, AuditEntry, AuditObject, AuditPage, AuditPolicy,
    AuditQuery, MailboxAudit,
};
use tp_contracts::auth::{MassRemoval, Rights, SendRight, SyncGate};
use tp_contracts::blob::{Acquire, BlobIndex, Commit, Compression};
use tp_contracts::bus::{Event, EventBus, EventKind, EventStream};
use tp_contracts::delivery::{
    DeliveryDecision, DeliveryEvent, DeliveryEventKind, DeliveryLog, DeliveryPage, DeliveryQuery,
};
use tp_contracts::events::{Partition, PartitionAssigner, ShardEventLogs, ShardPartition};
use tp_contracts::ext::{Decision, DecisionHook, DecisionHooks, FinalDelete};
use tp_contracts::groups::{
    DistributionGroup, GroupMember, GroupStore, HeldMessage, HoldSource, Moderation, Moderator,
    SenderRestriction,
};
use tp_contracts::identity::{Identity, IdentityAddress, IdentityStore};
use tp_contracts::membership::{Membership, NodeId, NodeInfo, SiteId};
use tp_contracts::meta::{
    Account, AccountScope, AccountState, AccountStore, AccountUpsert, ExistingContent,
    MailboxStore, MessageQuery, MessageStore, NewMailbox, NewMessage, PolicyResolver, SpecialUse,
};
use tp_contracts::queue::{
    DsnParams, OutMessage, OutRcpt, Outbound, QueueStore, RcptStatus, SubmitStatus,
};
use tp_contracts::retention::{
    Quota, Restored, TrashItem, TrashObject, TrashPurge, TrashPurgeRule, TrashPurgeSetting,
    TrashStore, trash_changed,
};
use tp_contracts::role::Role;
use tp_contracts::rpc::{Rpc, RpcError, RpcHandler, RpcRequest, RpcService};
use tp_contracts::secrets::{SecretLease, SecretRef, SecretStore};
use tp_contracts::senders::{SenderList, SenderLists, SenderVerdict};
use tp_contracts::submission::{EmailSubmission, SubmissionFilter, SubmissionStore, UndoStatus};
use tp_contracts::transport::{
    AcceptedDomain, AddressPattern, DestinationPolicy, DomainKind, EnvelopeSide, RewriteRule,
    SourceKind, SourceMatch, TenantTransport, TlsRequirement, TransportStore, TrustedSource,
};
use tp_contracts::{
    AccountId, BlobHash, BlobKey, ContactId, DomainId, Error, Gen, GroupId, HeldId, MailboxId,
    MessageId, OrgUnitId, QueueId, ShardId, TenantId,
};

use crate::ManualClock;
use crate::fakes::{HookCall, RecordingDecisionHook};

/// Ход времени для наборов тестов.
#[async_trait]
pub trait Wait: Send + Sync {
    /// Дождаться, пока пройдёт `d` по часам проверяемого хранилища.
    async fn wait(&self, d: Duration);
}

#[async_trait]
impl Wait for ManualClock {
    async fn wait(&self, d: Duration) {
        self.advance(d);
    }
}

/// Настоящее время: `tokio::time::sleep`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealTime;

#[async_trait]
impl Wait for RealTime {
    async fn wait(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

// ---------- Разделы журнала по шардам ----------

/// Набор проверок `PartitionAssigner` при текущем составе (CTR-34): каждый раздел `0..partitions` каждого шарда
/// `shards` (и [`ShardId::MAIN`]) назначен ровно одному узлу состава, списки — по возрастанию, один шард —
/// прежнее назначение `assigned`, узел вне состава — пусто.
pub fn partition_assigner(a: &dyn PartitionAssigner, shards: &[ShardId], partitions: u32) {
    let members = a.members().borrow().clone();
    assert!(!members.is_empty(), "состав не пуст");
    let mut wanted: Vec<ShardId> = shards.iter().copied().chain([ShardId::MAIN]).collect();
    wanted.sort();
    wanted.dedup();
    for subscriber in ["indexer", "mod_x"] {
        let mut all = Vec::new();
        for node in &members {
            let mine = a.assigned_sharded(subscriber, node, &wanted, partitions);
            assert!(mine.windows(2).all(|w| w[0] < w[1]), "по возрастанию");
            let main: Vec<Partition> = a
                .assigned_sharded(subscriber, node, &[ShardId::MAIN], partitions)
                .into_iter()
                .map(|p| p.index)
                .collect();
            assert_eq!(
                main,
                a.assigned(subscriber, node, partitions),
                "один шард — прежнее назначение"
            );
            all.extend(mine);
        }
        all.sort();
        let expected: Vec<ShardPartition> = wanted
            .iter()
            .flat_map(|s| (0..partitions).map(|p| ShardPartition::new(*s, Partition(p))))
            .collect();
        assert_eq!(
            all, expected,
            "каждый раздел каждого шарда — ровно одному узлу"
        );
        let outsider = NodeId::new("suite-outsider-node").expect("node");
        if !members.contains(&outsider) {
            assert!(
                a.assigned_sharded(subscriber, &outsider, &wanted, partitions)
                    .is_empty()
            );
        }
    }
}

/// Набор проверок `ShardEventLogs` (CTR-34): шарды по возрастанию с [`ShardId::MAIN`], журнал есть у каждого шарда
/// справочника и только у них, число разделов одинаково; позиции нового подписчика — с начала.
pub async fn shard_event_logs(logs: &dyn ShardEventLogs) {
    let shards = logs.shards();
    assert!(shards.contains(&ShardId::MAIN));
    assert!(shards.windows(2).all(|w| w[0] < w[1]), "по возрастанию");
    let partitions = logs.partitions();
    assert!(partitions >= 1);
    let subscriber = format!("suite-{}", AccountId::new());
    for shard in &shards {
        let log = logs.log(*shard).expect("журнал шарда справочника");
        assert_eq!(log.partitions(), partitions);
        for p in 0..partitions {
            assert_eq!(
                log.position(&subscriber, Partition(p))
                    .await
                    .expect("position"),
                tp_contracts::events::EventPos(0)
            );
        }
    }
    let unknown = ShardId(shards.last().map_or(0, |s| s.0).saturating_add(1));
    if !shards.contains(&unknown) {
        assert!(logs.log(unknown).is_none());
    }
}

// ---------- Membership ----------

/// Набор проверок `Membership`. `ttl` — срок записи узла; с [`RealTime`] — с запасом на задержки
/// хранилища (секунды). Набор не требует пустого состава: узлы набора — с уникальным префиксом,
/// чужие записи в `live()` не мешают.
pub async fn membership(m: &dyn Membership, ttl: Duration, wait: &dyn Wait) {
    let prefix = format!("suite-{}", TenantId::new());
    let node = |n: &str| NodeId::new(format!("{prefix}-{n}")).expect("node id");
    let (a, b, c) = (node("a"), node("b"), node("c"));
    let ours = || async {
        let live = m.live().await.expect("live");
        let mut sorted = live.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(live, sorted, "live(): по возрастанию без повторов");
        live.into_iter()
            .filter(|n| n.as_str().starts_with(&prefix))
            .collect::<Vec<_>>()
    };

    assert!(ours().await.is_empty(), "до heartbeat узлов набора нет");

    // Создание записи и повторное продление без повторов в составе.
    m.heartbeat(&NodeInfo::new(c.clone()), ttl)
        .await
        .expect("heartbeat c");
    m.heartbeat(&NodeInfo::new(a.clone()), ttl)
        .await
        .expect("heartbeat a");
    m.heartbeat(&NodeInfo::new(b.clone()), ttl)
        .await
        .expect("heartbeat b");
    m.heartbeat(&NodeInfo::new(a.clone()), ttl)
        .await
        .expect("heartbeat a again");
    assert_eq!(ours().await, vec![a.clone(), b.clone(), c.clone()]);

    // Выход — сразу, без ожидания срока; повторный выход и выход отсутствующего — не ошибка.
    m.leave(&c).await.expect("leave c");
    m.leave(&c).await.expect("leave c again");
    m.leave(&node("never")).await.expect("leave absent");
    assert_eq!(ours().await, vec![a.clone(), b.clone()]);

    // Продление переносит срок: a продлевается каждые ttl/2, b — нет.
    let half = ttl / 2;
    wait.wait(half).await;
    m.heartbeat(&NodeInfo::new(a.clone()), ttl)
        .await
        .expect("renew a");
    wait.wait(half + ttl / 4).await;
    assert_eq!(ours().await, vec![a.clone()], "b истёк, a продлён");

    // Без продления истекает и a; продление после истечения возвращает узел.
    wait.wait(ttl).await;
    assert!(ours().await.is_empty(), "все записи набора истекли");
    m.heartbeat(&NodeInfo::new(b.clone()), ttl)
        .await
        .expect("heartbeat b after expiry");
    assert_eq!(ours().await, vec![b.clone()]);
    m.leave(&b).await.expect("leave b");
    assert!(ours().await.is_empty());
}

/// Набор проверок записей узлов `Membership` ([`NodeInfo`]: роли, адрес, площадка) и `live_info()`.
/// Требования к `ttl` и составу — как у [`membership`].
pub async fn membership_info(m: &dyn Membership, ttl: Duration, wait: &dyn Wait) {
    let prefix = format!("suite-info-{}", TenantId::new());
    let ours = || async {
        let live = m.live_info().await.expect("live_info");
        let ids: Vec<NodeId> = live.iter().map(|n| n.id.clone()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ids, sorted, "live_info(): по возрастанию id без повторов");
        live.into_iter()
            .filter(|n| n.id.as_str().starts_with(&prefix))
            .collect::<Vec<_>>()
    };
    let mut a = NodeInfo::new(NodeId::new(format!("{prefix}-a")).expect("node id"));
    a.roles = [Role::Search, Role::Indexer].into_iter().collect();
    a.addr = "https://node-a.internal:7443".to_owned();
    a.site = SiteId::new("site-1").expect("site");
    let b = NodeInfo::new(NodeId::new(format!("{prefix}-b")).expect("node id"));

    m.heartbeat(&b, ttl).await.expect("heartbeat b");
    m.heartbeat(&a, ttl).await.expect("heartbeat a");
    assert_eq!(ours().await, vec![a.clone(), b.clone()]);
    let ids = m.live().await.expect("live");
    assert!(
        ids.contains(&a.id) && ids.contains(&b.id),
        "live() — те же узлы"
    );

    // Продление заменяет роли, адрес и площадку.
    let mut a2 = a.clone();
    a2.roles = [Role::Admin].into_iter().collect();
    a2.addr = "https://node-a.internal:8443".to_owned();
    a2.site = SiteId::default();
    m.heartbeat(&a2, ttl).await.expect("heartbeat a2");
    assert_eq!(ours().await, vec![a2.clone(), b.clone()]);

    // Выход и истечение убирают запись целиком.
    m.leave(&b.id).await.expect("leave b");
    assert_eq!(ours().await, vec![a2.clone()]);
    wait.wait(ttl + ttl / 4).await;
    assert!(ours().await.is_empty(), "запись a истекла");
}

// ---------- Календари и контакты, планирование встреч, Sieve (CTR-45…47) ----------

pub mod dav;
pub mod freebusy;
pub mod itip;
pub mod sieve;

pub use dav::dav;
pub use freebusy::freebusy;
pub use itip::itip;
pub use sieve::sieve;

// ---------- EventBus: сигналы ----------

/// Сколько ждать сигнала в наборе [`signals`].
const SIGNAL_WAIT: Duration = Duration::from_secs(5);

async fn next_signal(stream: &mut EventStream) -> Option<Event> {
    tokio::time::timeout(SIGNAL_WAIT, stream.next())
        .await
        .expect("сигнал не пришёл")
}

/// Набор проверок системных сигналов `EventBus` (CTR-23, CTR-37, CTR-39, CTR-49): `LogAppended`, `MembersChanged`,
/// `ShardsChanged`, `Blocked`, `Unblocked`, `TrashChanged` приходят
/// в `subscribe_system()` и не приходят подписке учётной записи; `publish(Resync)` — `Invalid`.
/// «Не пришло» проверяется тем, что первым пришёл сигнал, опубликованный позже.
pub async fn signals(bus: &dyn EventBus) {
    let scope = AccountScope {
        tenant: TenantId::new(),
        account: AccountId::new(),
    };
    let mut sys = bus.subscribe_system().await.expect("subscribe_system");
    let mut acc = bus.subscribe(scope).await.expect("subscribe");
    let system = |tenant, kind| Event {
        tenant,
        account: None,
        kind,
    };
    let published = [
        system(
            TenantId::DEFAULT,
            EventKind::LogAppended {
                partition: Partition(7),
            },
        ),
        system(TenantId::DEFAULT, EventKind::MembersChanged),
        system(TenantId::DEFAULT, EventKind::ShardsChanged),
        system(
            scope.tenant,
            EventKind::Blocked {
                subject: BlockSubject::net("198.51.100.0".parse().expect("ip"), 24).expect("net"),
            },
        ),
        system(
            scope.tenant,
            EventKind::Blocked {
                subject: BlockSubject::Account(scope.account),
            },
        ),
        system(
            scope.tenant,
            EventKind::Unblocked {
                subject: BlockSubject::Account(scope.account),
            },
        ),
        trash_changed(scope),
    ];
    for e in &published {
        assert!(e.kind.is_system());
        bus.publish(e.clone()).await.expect("publish");
    }
    let mine = Event {
        tenant: scope.tenant,
        account: Some(scope.account),
        kind: EventKind::MailboxListChanged,
    };
    bus.publish(mine.clone()).await.expect("publish");

    assert_eq!(
        next_signal(&mut acc).await,
        Some(mine),
        "системные — не подписке учётной записи"
    );
    for e in published {
        assert_eq!(next_signal(&mut sys).await, Some(e));
    }

    for resync in [
        system(TenantId::DEFAULT, EventKind::Resync),
        Event {
            account: Some(scope.account),
            ..system(scope.tenant, EventKind::Resync)
        },
    ] {
        assert!(
            matches!(bus.publish(resync).await, Err(Error::Invalid(_))),
            "Resync вставляет только шина"
        );
    }
}

// ---------- Rpc ----------

/// Узлы для [`rpc`]: не меньше двух, у всех — роль `role` и предел параллельности `max_in_flight`,
/// ни у одного — роли `absent_role`. Службы набора (`suite-*`) на узлах не зарегистрированы.
#[derive(Clone)]
pub struct RpcFixture {
    /// Узлы и их `Rpc`.
    pub nodes: Vec<(NodeId, Arc<dyn Rpc>)>,
    /// Роль всех узлов.
    pub role: Role,
    /// Роль, которой нет ни у одного узла.
    pub absent_role: Role,
    /// Предел параллельности обработки на узле.
    pub max_in_flight: usize,
}

impl std::fmt::Debug for RpcFixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcFixture")
            .field(
                "nodes",
                &self.nodes.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            )
            .field("role", &self.role)
            .field("absent_role", &self.absent_role)
            .field("max_in_flight", &self.max_in_flight)
            .finish()
    }
}

/// Эхо: `<узел>|<вызывающий>|<ключ>|<тело>`.
struct Echo(NodeId);

#[async_trait]
impl RpcHandler for Echo {
    async fn handle(&self, r: RpcRequest) -> tp_contracts::Result<Bytes> {
        let body = String::from_utf8_lossy(&r.body);
        Ok(Bytes::from(format!(
            "{}|{}|{}|{body}",
            self.0, r.caller, r.key
        )))
    }
}

struct Fail;

#[async_trait]
impl RpcHandler for Fail {
    async fn handle(&self, _: RpcRequest) -> tp_contracts::Result<Bytes> {
        Err(Error::invalid("suite failure"))
    }
}

/// Ждёт открытия ворот; `entered` — сколько обработчиков вошло.
struct Gate {
    entered: Arc<std::sync::atomic::AtomicUsize>,
    open: Arc<tokio::sync::Semaphore>,
}

#[async_trait]
impl RpcHandler for Gate {
    async fn handle(&self, _: RpcRequest) -> tp_contracts::Result<Bytes> {
        self.entered
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _permit = self
            .open
            .acquire()
            .await
            .map_err(|_| Error::internal("gate closed"))?;
        Ok(Bytes::from_static(b"done"))
    }
}

/// Отвечает через `delay` (дольше срока вызова в наборе).
struct Slow(Duration);

#[async_trait]
impl RpcHandler for Slow {
    async fn handle(&self, _: RpcRequest) -> tp_contracts::Result<Bytes> {
        tokio::time::sleep(self.0).await;
        Ok(Bytes::new())
    }
}

/// Набор проверок `Rpc`: маршрут к владельцу ключа (один и тот же с любого узла), ошибки обработчика,
/// `Unavailable` без узлов роли, `Busy` при превышении параллельности, `Timeout`, повторная регистрация.
/// Время — настоящее (короткие сроки, без долгого сна).
pub async fn rpc(fx: &RpcFixture) {
    assert!(fx.nodes.len() >= 2, "набору нужно не меньше двух узлов");
    let svc = |name: &str| RpcService::new(fx.role, name).expect("service");
    let (echo, fail, gate, slow) = (
        svc("suite-echo"),
        svc("suite-fail"),
        svc("suite-gate"),
        svc("suite-slow"),
    );
    let long = Duration::from_secs(10);
    let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let open = Arc::new(tokio::sync::Semaphore::new(0));
    for (id, rpc) in &fx.nodes {
        rpc.serve(echo.clone(), Arc::new(Echo(id.clone())))
            .expect("serve echo");
        rpc.serve(fail.clone(), Arc::new(Fail)).expect("serve fail");
        rpc.serve(
            gate.clone(),
            Arc::new(Gate {
                entered: entered.clone(),
                open: open.clone(),
            }),
        )
        .expect("serve gate");
        rpc.serve(slow.clone(), Arc::new(Slow(Duration::from_millis(500))))
            .expect("serve slow");
    }
    assert_eq!(
        fx.nodes[0].1.serve(echo.clone(), Arc::new(Fail)),
        Err(Error::Conflict),
        "повторная регистрация службы"
    );

    // Маршрут: владелец ключа один и тот же с любого узла; ключи расходятся по разным узлам.
    let mut owners = std::collections::BTreeSet::new();
    for k in 0..24 {
        let key = format!("suite/{k}");
        let mut seen = None;
        for (caller, rpc) in &fx.nodes {
            let out = rpc
                .call(&echo, &key, Bytes::from_static(b"ping"), long)
                .await
                .expect("echo");
            let out = String::from_utf8(out.to_vec()).expect("utf8");
            let parts: Vec<&str> = out.split('|').collect();
            assert_eq!(parts.len(), 4, "{out}");
            assert_eq!(parts[1..], [caller.as_str(), key.as_str(), "ping"], "{out}");
            let owner = parts[0].to_owned();
            assert!(
                seen.get_or_insert_with(|| owner.clone()) == &owner,
                "владелец {key} зависит от вызывающего"
            );
            owners.insert(owner);
        }
    }
    assert!(
        owners.len() > 1,
        "24 ключа достались одному узлу: {owners:?}"
    );

    // Ошибка обработчика и отсутствие узлов роли.
    let caller = &fx.nodes[0].1;
    assert_eq!(
        caller.call(&fail, "suite/x", Bytes::new(), long).await,
        Err(RpcError::Failed(Error::invalid("suite failure")))
    );
    let none = RpcService::new(fx.absent_role, "suite-none").expect("service");
    assert_eq!(
        caller.call(&none, "suite/x", Bytes::new(), long).await,
        Err(RpcError::Unavailable)
    );

    // Busy: владелец ключа занят `max_in_flight` запросами — следующий получает отказ «занят».
    let mut pending = Vec::new();
    for _ in 0..fx.max_in_flight {
        let (rpc, gate) = (caller.clone(), gate.clone());
        pending.push(tokio::spawn(async move {
            rpc.call(&gate, "suite/busy", Bytes::new(), long).await
        }));
    }
    let started = tokio::time::Instant::now();
    while entered.load(std::sync::atomic::Ordering::SeqCst) < fx.max_in_flight {
        assert!(started.elapsed() < long, "обработчики не начали работу");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        caller.call(&gate, "suite/busy", Bytes::new(), long).await,
        Err(RpcError::Busy)
    );
    open.add_permits(fx.max_in_flight);
    for p in pending {
        assert_eq!(p.await.expect("join"), Ok(Bytes::from_static(b"done")));
    }

    // Timeout: обработчик отвечает позже срока.
    assert_eq!(
        caller
            .call(&slow, "suite/slow", Bytes::new(), Duration::from_millis(50))
            .await,
        Err(RpcError::Timeout)
    );
}

// ---------- AbuseBlocks ----------

/// База сроков набора: `now`, округлённый вверх до целой секунды (PostgreSQL хранит микросекунды; база не раньше
/// `now`, поэтому `base + ttl / 2` не в прошлом), и её опережение `now` (меньше секунды).
fn whole_second_after(now: OffsetDateTime) -> (OffsetDateTime, Duration) {
    let floor = now.replace_nanosecond(0).expect("ns");
    let base = if floor == now {
        now
    } else {
        floor + Duration::from_secs(1)
    };
    (base, (base - now).unsigned_abs())
}

/// Набор проверок `AbuseBlocks`. `now` — текущий момент по часам хранилища, `ttl` — срок блокировок набора
/// (с [`RealTime`] — с запасом на задержки). Организации набора — новые.
pub async fn abuse_blocks(
    blocks: &dyn AbuseBlocks,
    now: OffsetDateTime,
    ttl: Duration,
    wait: &dyn Wait,
) {
    let (base, lead) = whole_second_after(now);
    let at = |d: Duration| base + d;
    let (tenant, other) = (TenantId::new(), TenantId::new());
    let ip = |s: &str| BlockSubject::Ip(s.parse().expect("ip"));
    let src = ip("203.0.113.7");
    let in_net = ip("198.51.100.20");
    let outside = ip("198.51.100.200");
    let net = BlockSubject::net("198.51.100.0".parse().expect("ip"), 27).expect("net");
    let account = BlockSubject::Account(AccountId::new());

    assert_eq!(blocks.blocked(tenant, &src).await, Ok(None));
    assert_eq!(blocks.active(tenant).await, Ok(Vec::new()));
    assert!(matches!(
        blocks
            .block(
                tenant,
                &src,
                at(Duration::ZERO) - Duration::from_secs(1),
                "past"
            )
            .await,
        Err(Error::Invalid(_))
    ));

    // Блокировка видна только в своей организации; продление не сокращает срок.
    blocks
        .block(tenant, &src, at(ttl), "r1")
        .await
        .expect("block");
    assert_eq!(blocks.blocked(tenant, &src).await, Ok(Some(at(ttl))));
    assert_eq!(blocks.blocked(other, &src).await, Ok(None));
    blocks
        .block(tenant, &src, at(ttl / 2), "r2")
        .await
        .expect("shorter");
    assert_eq!(blocks.blocked(tenant, &src).await, Ok(Some(at(ttl))));
    blocks
        .block(tenant, &src, at(ttl * 3), "r3")
        .await
        .expect("longer");
    assert_eq!(blocks.blocked(tenant, &src).await, Ok(Some(at(ttl * 3))));

    // Подсеть покрывает адреса внутри; учётная запись — только себя.
    blocks
        .block(tenant, &net, at(ttl), "net")
        .await
        .expect("net");
    blocks
        .block(tenant, &account, at(ttl), "acc")
        .await
        .expect("account");
    assert_eq!(blocks.blocked(tenant, &in_net).await, Ok(Some(at(ttl))));
    assert_eq!(blocks.blocked(tenant, &outside).await, Ok(None));
    assert_eq!(blocks.blocked(tenant, &net).await, Ok(Some(at(ttl))));
    assert_eq!(blocks.blocked(tenant, &account).await, Ok(Some(at(ttl))));
    assert_eq!(
        blocks
            .blocked(tenant, &BlockSubject::Account(AccountId::new()))
            .await,
        Ok(None)
    );
    let mut active = blocks.active(tenant).await.expect("active");
    active.sort_by_key(|b| b.subject.to_string());
    let expect = |subject, until, reason: &str| AbuseBlock {
        subject,
        until,
        reason: reason.to_owned(),
    };
    let mut want = vec![
        expect(src, at(ttl * 3), "r3"),
        expect(net, at(ttl), "net"),
        expect(account, at(ttl), "acc"),
    ];
    want.sort_by_key(|b| b.subject.to_string());
    assert_eq!(active, want);
    assert_eq!(blocks.active(other).await, Ok(Vec::new()));

    // Снятие — только этого субъекта; отсутствующего — не ошибка.
    blocks.unblock(tenant, &src).await.expect("unblock");
    blocks.unblock(tenant, &src).await.expect("unblock again");
    blocks
        .unblock(tenant, &in_net)
        .await
        .expect("unblock not blocked");
    assert_eq!(blocks.blocked(tenant, &src).await, Ok(None));
    assert_eq!(blocks.blocked(tenant, &in_net).await, Ok(Some(at(ttl))));

    // Истёкшие блокировки не действуют; новая блокировка после истечения — с новым сроком.
    wait.wait(lead + ttl + ttl / 4).await;
    assert_eq!(blocks.blocked(tenant, &in_net).await, Ok(None));
    assert_eq!(blocks.blocked(tenant, &account).await, Ok(None));
    assert_eq!(blocks.active(tenant).await, Ok(Vec::new()));
    blocks
        .block(tenant, &account, at(ttl * 3), "again")
        .await
        .expect("again");
    assert_eq!(
        blocks.blocked(tenant, &account).await,
        Ok(Some(at(ttl * 3)))
    );
}

/// Набор проверок сигналов `AbuseBlocks` (CTR-39): `blocks` публикует в `bus` (шину своей реализации)
/// `Blocked { subject }` после `block` и `Unblocked { subject }` после `unblock` действующей блокировки, с
/// организацией блокировки и `account = None`. `now` и `ttl` — как в [`abuse_blocks`]. Организация набора — новая.
pub async fn abuse_blocks_signals(
    blocks: &dyn AbuseBlocks,
    bus: &dyn EventBus,
    now: OffsetDateTime,
    ttl: Duration,
) {
    let (base, _) = whole_second_after(now);
    let tenant = TenantId::new();
    let mut sys = bus.subscribe_system().await.expect("subscribe_system");
    let signal = |kind| Event {
        tenant,
        account: None,
        kind,
    };
    for subject in [
        BlockSubject::Ip("203.0.113.9".parse().expect("ip")),
        BlockSubject::net("2001:db8::".parse().expect("ip"), 48).expect("net"),
        BlockSubject::Account(AccountId::new()),
    ] {
        blocks
            .block(tenant, &subject, base + ttl, "signals")
            .await
            .expect("block");
        assert_eq!(
            next_signal(&mut sys).await,
            Some(signal(EventKind::Blocked { subject }))
        );
        blocks.unblock(tenant, &subject).await.expect("unblock");
        assert_eq!(
            next_signal(&mut sys).await,
            Some(signal(EventKind::Unblocked { subject })),
            "снятие блокировки — сигнал"
        );
        assert_eq!(blocks.blocked(tenant, &subject).await, Ok(None));
    }
}

// ---------- SecretStore ----------

/// Данные хранилища для [`secret_store`]; хранилище заполняет тест до запуска набора.
#[derive(Debug, Clone)]
pub struct SecretFixture {
    /// Статический секрет и его значение.
    pub present: (SecretRef, String),
    /// Поле, которого нет у существующего пути.
    pub missing_field: SecretRef,
    /// Несуществующий путь.
    pub missing_path: SecretRef,
    /// Ссылка, запрещённая политикой, если хранилище умеет политики.
    pub forbidden: Option<SecretRef>,
    /// Динамический секрет с продлеваемой арендой (срок — короткий для [`RealTime`]), если есть.
    pub dynamic: Option<SecretRef>,
}

/// Набор проверок `SecretStore` на подготовленных данных `fixture`.
pub async fn secret_store(store: &dyn SecretStore, fixture: &SecretFixture, wait: &dyn Wait) {
    let (present, value) = &fixture.present;
    let s = store.get(present).await.expect("present secret");
    assert_eq!(s.expose(), value);
    assert!(s.lease().is_none(), "статический секрет — без аренды");
    let shown = format!("{s:?} {s}");
    assert!(!shown.contains(value.as_str()), "значение скрыто: {shown}");

    for missing in [&fixture.missing_field, &fixture.missing_path] {
        assert_eq!(
            store.get(missing).await.map(|_| ()),
            Err(Error::NotFound),
            "{missing}"
        );
    }
    if let Some(forbidden) = &fixture.forbidden {
        assert_eq!(
            store.get(forbidden).await.map(|_| ()),
            Err(Error::Forbidden),
            "{forbidden}"
        );
    }

    let unknown = SecretLease {
        id: format!("suite/unknown/{}", TenantId::new()),
        ttl: Duration::from_secs(60),
        renewable: true,
    };
    assert_eq!(store.renew(&unknown).await, Err(Error::NotFound));

    let Some(dynamic) = &fixture.dynamic else {
        return;
    };
    let s = store.get(dynamic).await.expect("dynamic secret");
    let lease = s.lease().cloned().expect("динамический секрет — с арендой");
    assert!(lease.renewable && !lease.ttl.is_zero(), "{lease:?}");
    assert_eq!(s.lease_ttl(), Some(lease.ttl));

    // Продление до истечения переносит срок от текущего момента.
    wait.wait(lease.ttl / 2).await;
    let renewed = store.renew(&lease).await.expect("renew");
    assert_eq!(renewed.id, lease.id);
    assert!(renewed.renewable && !renewed.ttl.is_zero(), "{renewed:?}");
    wait.wait(renewed.ttl * 3 / 4).await;
    let renewed = store
        .renew(&renewed)
        .await
        .expect("renew after first renew");

    // Истёкшая аренда — `NotFound`, секрет читается заново с новой арендой.
    wait.wait(renewed.ttl + renewed.ttl / 4).await;
    assert_eq!(store.renew(&renewed).await, Err(Error::NotFound));
    let again = store.get(dynamic).await.expect("dynamic again");
    assert!(again.lease().is_some_and(|l| l.id != lease.id));
}

// ---------- Системные папки ----------

/// Набор проверок системных папок `MailboxStore::list` (KIT-08, META-42; решение #205): недостающая
/// системная папка определяется только по роли и создаётся всегда; имя, занятое папкой пользователя
/// верхнего уровня (без учёта регистра ASCII), получает наименьший свободный суффикс.
///
/// `scope` — учётная запись без папки с ролью Junk, с папками верхнего уровня «Junk» и «junk 1» без роли
/// (остальные папки — любые).
pub async fn system_folders_by_role(mailboxes: &dyn MailboxStore, scope: AccountScope) {
    let first = mailboxes.list(scope).await.expect("list");
    // Каждая роль — ровно у одной папки.
    for role in [
        SpecialUse::Inbox,
        SpecialUse::Drafts,
        SpecialUse::Sent,
        SpecialUse::Junk,
        SpecialUse::Trash,
        SpecialUse::Archive,
    ] {
        assert_eq!(
            first.iter().filter(|m| m.role == Some(role)).count(),
            1,
            "роль {role:?}"
        );
    }
    // Системная Junk — верхнего уровня, с суффиксом после занятых «Junk» и «junk 1».
    let junk = first
        .iter()
        .find(|m| m.role == Some(SpecialUse::Junk))
        .expect("Junk");
    assert_eq!((junk.parent, junk.name.as_str()), (None, "Junk 2"));
    // Папки пользователя не тронуты: имя то же, роли нет.
    for name in ["Junk", "junk 1"] {
        let user = first
            .iter()
            .find(|m| m.parent.is_none() && m.name == name)
            .expect("папка пользователя");
        assert_eq!(user.role, None, "{name}");
    }
    // Повторное открытие — без новых папок.
    let ids = |v: &[tp_contracts::meta::Mailbox]| {
        let mut ids: Vec<MailboxId> = v.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        ids
    };
    let again = mailboxes.list(scope).await.expect("list");
    assert_eq!(ids(&again), ids(&first));
}

// ---------- AclStore ----------

/// Учётные записи для [`acl_store`]; создаёт тест до запуска набора (у каждой — системные папки).
#[derive(Debug, Clone, Copy)]
pub struct AclFixture {
    /// Владелец папок.
    pub owner: AccountScope,
    /// Другая учётная запись той же организации.
    pub peer: AccountScope,
    /// Учётная запись другой организации.
    pub foreign: AccountScope,
}

/// Набор проверок `AclStore`; папки создаёт и удаляет через `mailboxes`. Идентификаторы — с уникальным
/// суффиксом: записи других тестов той же организации не мешают.
pub async fn acl_store(acl: &dyn AclStore, mailboxes: &dyn MailboxStore, fx: &AclFixture) {
    let sfx = TenantId::new().to_string();
    let id = |n: &str| format!("{n}-{sfx}");
    let ids = |list: &[&str]| list.iter().map(|n| id(n)).collect::<Vec<_>>();
    let r = |s: &str| s.parse::<Rights>().expect("rights");
    let (fred, sales, assistants) = (id("fred"), id("sales"), id("assistants"));
    let not_fred = format!("-{fred}");
    let owner = fx.owner;
    let folder = |scope: AccountScope, name: &str| {
        let name = format!("{name}-{sfx}");
        async move {
            mailboxes
                .create(
                    scope,
                    NewMailbox {
                        parent: None,
                        name,
                        role: None,
                    },
                )
                .await
                .expect("create mailbox")
                .id
        }
    };
    let inbox = mailboxes
        .list(owner)
        .await
        .expect("list")
        .into_iter()
        .find(|m| m.role == Some(SpecialUse::Inbox))
        .expect("inbox")
        .id;
    let shared = folder(owner, "Shared").await;
    let peer_box = folder(fx.peer, "Peer").await;

    // Чтение и запись ACL; замена прав целиком; порядок — по идентификатору.
    assert!(acl.acl(owner, inbox).await.expect("acl").is_empty());
    for (who, rights) in [
        (fred.as_str(), "lrs"),
        (sales.as_str(), "lrswi"),
        (ANYONE, "l"),
        (not_fred.as_str(), "w"),
        (fred.as_str(), "lrsp"),
    ] {
        acl.set_acl(owner, inbox, who, r(rights))
            .await
            .expect("set_acl");
    }
    let mut expected = vec![
        AclEntry {
            identifier: not_fred.clone(),
            rights: r("w"),
        },
        AclEntry {
            identifier: ANYONE.to_owned(),
            rights: r("l"),
        },
        AclEntry {
            identifier: fred.clone(),
            rights: r("lrsp"),
        },
        AclEntry {
            identifier: sales.clone(),
            rights: r("lrswi"),
        },
    ];
    expected.sort_by(|a, b| a.identifier.cmp(&b.identifier));
    assert_eq!(acl.acl(owner, inbox).await.expect("acl"), expected);

    // Действующие права: объединение своих, групп и anyone минус отрицательные.
    let rights = |mailbox: MailboxId, who: Vec<String>| async move {
        acl.acl_rights(owner, mailbox, &who).await.expect("rights")
    };
    assert_eq!(rights(inbox, ids(&["fred", "sales"])).await, r("lrsip"));
    assert_eq!(rights(inbox, ids(&["fred"])).await, r("lrsp"));
    assert_eq!(rights(inbox, ids(&["bob"])).await, r("l"), "anyone");
    assert_eq!(rights(inbox, Vec::new()).await, r("l"), "только anyone");
    assert_eq!(rights(shared, ids(&["fred"])).await, Rights::NONE);
    assert_eq!(rights(MailboxId::new(), ids(&["fred"])).await, Rights::NONE);
    assert!(matches!(
        acl.acl_rights(owner, inbox, std::slice::from_ref(&not_fred))
            .await,
        Err(Error::Invalid(_))
    ));

    // Общие папки: только с правом l, по возрастанию (учётная запись, папка).
    let shared_of = |who: Vec<String>| async move {
        acl.shared_mailboxes(owner.tenant, &who)
            .await
            .expect("shared")
    };
    acl.set_acl(owner, shared, &fred, r("r"))
        .await
        .expect("без l");
    assert_eq!(
        shared_of(ids(&["fred"])).await,
        vec![SharedMailbox {
            account: owner.account,
            mailbox: inbox,
            rights: r("lrsp"),
        }]
    );
    acl.set_acl(owner, shared, &fred, r("lr"))
        .await
        .expect("с l");
    acl.set_acl(fx.peer, peer_box, &sales, r("lre"))
        .await
        .expect("peer");
    let mut expected = vec![
        SharedMailbox {
            account: owner.account,
            mailbox: inbox,
            rights: r("lrsip"),
        },
        SharedMailbox {
            account: owner.account,
            mailbox: shared,
            rights: r("lr"),
        },
        SharedMailbox {
            account: fx.peer.account,
            mailbox: peer_box,
            rights: r("lre"),
        },
    ];
    expected.sort_by_key(|s| (s.account, s.mailbox));
    assert_eq!(shared_of(ids(&["fred", "sales"])).await, expected);
    assert!(matches!(
        acl.shared_mailboxes(owner.tenant, std::slice::from_ref(&not_fred))
            .await,
        Err(Error::Invalid(_))
    ));

    // Пустые права удаляют запись (повторно — не ошибка); удаление папки удаляет её ACL.
    for _ in 0..2 {
        acl.set_acl(owner, inbox, &not_fred, Rights::NONE)
            .await
            .expect("delete entry");
    }
    assert_eq!(rights(inbox, ids(&["fred"])).await, r("lrsp"));
    mailboxes
        .delete(owner, shared, false)
        .await
        .expect("delete mailbox");
    assert_eq!(acl.acl(owner, shared).await, Err(Error::NotFound));
    assert_eq!(rights(shared, ids(&["fred"])).await, Rights::NONE);
    let left: Vec<MailboxId> = shared_of(ids(&["fred"]))
        .await
        .iter()
        .map(|s| s.mailbox)
        .collect();
    assert_eq!(left, vec![inbox]);

    // Ошибки: нет папки (и папка другой учётной записи); недопустимый идентификатор проверяется первым.
    for mailbox in [MailboxId::new(), peer_box] {
        assert_eq!(acl.acl(owner, mailbox).await, Err(Error::NotFound));
        assert_eq!(
            acl.set_acl(owner, mailbox, &fred, r("l")).await,
            Err(Error::NotFound)
        );
    }
    let long = "x".repeat(256);
    for bad in ["", "-", "a b", "a\tb", long.as_str()] {
        for mailbox in [inbox, MailboxId::new()] {
            assert!(
                matches!(
                    acl.set_acl(owner, mailbox, bad, r("l")).await,
                    Err(Error::Invalid(_))
                ),
                "{bad:?}"
            );
        }
    }

    // Другая организация не видит ACL.
    let alien = AccountScope {
        tenant: fx.foreign.tenant,
        account: owner.account,
    };
    assert_eq!(acl.acl(alien, inbox).await, Err(Error::NotFound));
    assert_eq!(
        acl.set_acl(alien, inbox, &fred, r("l")).await,
        Err(Error::NotFound)
    );
    assert!(
        acl.shared_mailboxes(fx.foreign.tenant, &ids(&["fred", "sales"]))
            .await
            .expect("foreign")
            .is_empty()
    );

    // ---- Делегирование отправки ----
    let right_of = |who: Vec<String>| async move {
        acl.send_right_of(owner, &who).await.expect("send_right_of")
    };
    assert!(acl.send_delegations(owner).await.expect("list").is_empty());
    assert_eq!(right_of(ids(&["fred"])).await, None);
    acl.set_send_delegation(owner, &fred, Some(SendRight::SendOnBehalf))
        .await
        .expect("fred");
    acl.set_send_delegation(owner, &assistants, Some(SendRight::SendAs))
        .await
        .expect("group");
    assert_eq!(
        right_of(ids(&["fred"])).await,
        Some(SendRight::SendOnBehalf)
    );
    // Send As через группу сильнее Send on Behalf; anyone не добавляется.
    assert_eq!(
        right_of(ids(&["fred", "assistants"])).await,
        Some(SendRight::SendAs)
    );
    assert_eq!(right_of(Vec::new()).await, None);
    let mut expected = vec![
        SendDelegation {
            delegate: assistants.clone(),
            right: SendRight::SendAs,
        },
        SendDelegation {
            delegate: fred.clone(),
            right: SendRight::SendOnBehalf,
        },
    ];
    expected.sort_by(|a, b| a.delegate.cmp(&b.delegate));
    assert_eq!(acl.send_delegations(owner).await.expect("list"), expected);
    assert!(
        acl.send_delegations(fx.peer)
            .await
            .expect("peer")
            .is_empty()
    );

    // Замена и отзыв (повторный отзыв — не ошибка).
    acl.set_send_delegation(owner, &fred, Some(SendRight::SendAs))
        .await
        .expect("upgrade");
    assert_eq!(right_of(ids(&["fred"])).await, Some(SendRight::SendAs));
    for _ in 0..2 {
        acl.set_send_delegation(owner, &fred, None)
            .await
            .expect("revoke");
    }
    assert_eq!(right_of(ids(&["fred"])).await, None);
    assert_eq!(acl.send_delegations(owner).await.expect("list").len(), 1);

    // Ошибки: нет учётной записи, недопустимый идентификатор; другая организация не видит делегирований.
    let missing = AccountScope {
        tenant: owner.tenant,
        account: AccountId::new(),
    };
    assert_eq!(
        acl.set_send_delegation(missing, &fred, Some(SendRight::SendAs))
            .await,
        Err(Error::NotFound)
    );
    for bad in ["", "-fred", "a b"] {
        assert!(matches!(
            acl.set_send_delegation(owner, bad, Some(SendRight::SendAs))
                .await,
            Err(Error::Invalid(_))
        ));
    }
    assert!(matches!(
        acl.send_right_of(owner, std::slice::from_ref(&not_fred))
            .await,
        Err(Error::Invalid(_))
    ));
    assert!(
        acl.send_delegations(alien)
            .await
            .expect("foreign")
            .is_empty()
    );
    assert_eq!(
        acl.set_send_delegation(alien, &fred, Some(SendRight::SendAs))
            .await,
        Err(Error::NotFound)
    );
    assert_eq!(acl.send_right_of(alien, &[assistants]).await, Ok(None));
}

// ---------- AppPasswordStore ----------

/// Учётные записи для [`app_password_store`]; создаёт тест до запуска набора.
#[derive(Debug, Clone, Copy)]
pub struct AppPasswordFixture {
    /// Владелец паролей (без паролей до набора).
    pub owner: AccountScope,
    /// Другая учётная запись той же организации (без паролей до набора).
    pub peer: AccountScope,
    /// Другая организация.
    pub foreign_tenant: TenantId,
}

/// Набор проверок `AppPasswordStore`. Моменты — целые секунды (точность хранилища — не грубее микросекунды).
pub async fn app_password_store(store: &dyn AppPasswordStore, fx: &AppPasswordFixture) {
    let at = |s: i64| OffsetDateTime::from_unix_timestamp(1_800_000_000 + s).expect("time");
    let pw = |id: &str, created: i64| AppPassword {
        id: id.to_owned(),
        label: format!("Телефон {id}"),
        hash: format!("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA${id}"),
        created_at: at(created),
        revoked_at: None,
    };
    let owner = fx.owner;
    let list = |scope: AccountScope| async move { store.list(scope).await.expect("list") };
    assert!(list(owner).await.is_empty());

    // Порядок — по created_at, затем по id; отозванный при добавлении сохраняется как есть.
    let (b, z, a) = (pw("b", 10), pw("z", 0), pw("a", 10));
    let old = AppPassword {
        revoked_at: Some(at(5)),
        ..pw("old", -10)
    };
    for p in [&b, &z, &a, &old] {
        store.insert(owner, p.clone()).await.expect("insert");
    }
    assert_eq!(
        list(owner).await,
        vec![old.clone(), z.clone(), a.clone(), b.clone()]
    );

    // Идентификатор занят — Conflict; в другой учётной записи тот же идентификатор свободен.
    assert_eq!(store.insert(owner, pw("a", 20)).await, Err(Error::Conflict));
    assert!(list(fx.peer).await.is_empty());
    store.insert(fx.peer, pw("a", 20)).await.expect("peer");
    assert_eq!(list(fx.peer).await, vec![pw("a", 20)]);

    // Недопустимые поля — Invalid (раньше NotFound); нет учётной записи — NotFound.
    let missing = AccountScope {
        tenant: owner.tenant,
        account: AccountId::new(),
    };
    let bad = [
        AppPassword {
            id: String::new(),
            ..pw("x", 0)
        },
        AppPassword {
            id: "a b".into(),
            ..pw("x", 0)
        },
        AppPassword {
            hash: String::new(),
            ..pw("x", 0)
        },
        AppPassword {
            label: "a\nb".into(),
            ..pw("x", 0)
        },
    ];
    for p in bad {
        for scope in [owner, missing] {
            assert!(
                matches!(store.insert(scope, p.clone()).await, Err(Error::Invalid(_))),
                "{p:?}"
            );
        }
    }
    assert_eq!(
        store.insert(missing, pw("x", 0)).await,
        Err(Error::NotFound)
    );
    assert!(list(missing).await.is_empty());

    // Отзыв: момент первого отзыва сохраняется; нет пароля — NotFound.
    store.revoke(owner, "a", at(30)).await.expect("revoke");
    store
        .revoke(owner, "a", at(40))
        .await
        .expect("revoke again");
    store
        .revoke(owner, "old", at(50))
        .await
        .expect("revoke revoked");
    let revoked = AppPassword {
        revoked_at: Some(at(30)),
        ..a.clone()
    };
    assert_eq!(list(owner).await, vec![old, z, revoked, b]);
    assert_eq!(
        list(fx.peer).await,
        vec![pw("a", 20)],
        "отзыв не задел peer"
    );
    assert_eq!(
        store.revoke(owner, "nope", at(30)).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.revoke(fx.peer, "b", at(30)).await,
        Err(Error::NotFound)
    );

    // Другая организация не видит пароли.
    let alien = AccountScope {
        tenant: fx.foreign_tenant,
        account: owner.account,
    };
    assert!(list(alien).await.is_empty());
    assert_eq!(store.revoke(alien, "b", at(30)).await, Err(Error::NotFound));
    assert_eq!(store.insert(alien, pw("c", 0)).await, Err(Error::NotFound));
    assert_eq!(list(owner).await.len(), 4);
}

// ---------- LocalAdminTokens ----------

/// Набор проверок `LocalAdminTokens` (CTR-41): выпуск, однократное погашение (в том числе параллельное), срок, отзыв,
/// список организации и очистка. `tenant` — организация без токенов (новая для набора); идентификаторы и хэши —
/// уникальные, чужие токены хранилища не мешают.
pub async fn local_admin_tokens(store: &dyn LocalAdminTokens, tenant: TenantId) {
    // Целые секунды: PostgreSQL хранит микросекунды.
    let now = OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp())
        .expect("now");
    let ttl = Duration::from_secs(600);
    let run = AccountId::new();
    let account = AccountId::new();
    let hash = || {
        let (a, b) = (AccountId::new(), AccountId::new());
        format!(
            "{:032x}{:032x}",
            a.as_uuid().as_u128(),
            b.as_uuid().as_u128()
        )
    };
    let token = |n: &str, created_at: OffsetDateTime| LocalAdminToken {
        id: format!("{run}-{n}"),
        tenant,
        account,
        hash: hash(),
        created_at,
        expires_at: created_at + ttl,
        used_at: None,
        revoked_at: None,
    };

    // Выпуск: занятые идентификатор и хэш — Conflict, недопустимые поля — Invalid.
    let a = token("a", now);
    store.issue(a.clone()).await.expect("issue");
    let same_id = LocalAdminToken {
        hash: hash(),
        ..a.clone()
    };
    assert_eq!(store.issue(same_id).await, Err(Error::Conflict));
    let same_hash = LocalAdminToken {
        id: format!("{run}-a2"),
        ..a.clone()
    };
    assert_eq!(store.issue(same_hash).await, Err(Error::Conflict));
    let bad = LocalAdminToken {
        hash: "not-a-hash".into(),
        ..token("bad", now)
    };
    assert!(matches!(store.issue(bad).await, Err(Error::Invalid(_))));
    let used = LocalAdminToken {
        used_at: Some(now),
        ..token("used", now)
    };
    assert!(matches!(store.issue(used).await, Err(Error::Invalid(_))));

    // Погашение: один раз; неизвестный хэш — Forbidden.
    assert_eq!(store.consume(&hash(), now).await, Err(Error::Forbidden));
    let at = now + Duration::from_secs(5);
    let got = store.consume(&a.hash, at).await.expect("consume");
    assert_eq!(
        got,
        LocalAdminToken {
            used_at: Some(at),
            ..a.clone()
        }
    );
    assert_eq!(store.consume(&a.hash, at).await, Err(Error::Forbidden));

    // Параллельные погашения — успешно ровно одно.
    let b = token("b", now + Duration::from_secs(1));
    store.issue(b.clone()).await.expect("issue b");
    let (r1, r2) = futures::join!(store.consume(&b.hash, at), store.consume(&b.hash, at));
    assert_eq!(
        [&r1, &r2].iter().filter(|r| r.is_ok()).count(),
        1,
        "{r1:?} {r2:?}"
    );

    // Срок: в момент expires_at и позже — Forbidden.
    let c = token("c", now + Duration::from_secs(2));
    store.issue(c.clone()).await.expect("issue c");
    assert_eq!(
        store.consume(&c.hash, c.expires_at).await,
        Err(Error::Forbidden)
    );

    // Отзыв: нет — NotFound, повтор сохраняет первый момент, отозванный не погашается.
    let d = token("d", now + Duration::from_secs(3));
    store.issue(d.clone()).await.expect("issue d");
    assert_eq!(
        store.revoke(&format!("{run}-none"), at).await,
        Err(Error::NotFound)
    );
    store.revoke(&d.id, at).await.expect("revoke");
    store
        .revoke(&d.id, at + Duration::from_secs(1))
        .await
        .expect("revoke again");
    assert_eq!(store.consume(&d.hash, at).await, Err(Error::Forbidden));
    store.revoke(&a.id, at).await.expect("revoke used");

    // Список организации: все состояния, по (created_at, id); чужая организация не видит.
    let list = store.list(tenant).await.expect("list");
    let ids: Vec<&str> = list.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, [&a.id, &b.id, &c.id, &d.id]);
    assert_eq!(list[3].revoked_at, Some(at));
    assert_eq!(list[0].used_at, Some(at));
    assert_eq!(list[0].revoked_at, Some(at));
    assert!(list[1].used_at.is_some());
    assert!(
        store
            .list(TenantId::new())
            .await
            .expect("foreign")
            .is_empty()
    );

    // Очистка: удаляются только с expires_at < before.
    let removed = store.prune(c.expires_at).await.expect("prune");
    assert!(removed >= 2, "{removed}");
    let ids: Vec<String> = store
        .list(tenant)
        .await
        .expect("list")
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, [c.id.clone(), d.id.clone()]);
}

// ---------- IdentityStore, SubmissionStore ----------

/// Набор проверок `IdentityStore` (CTR-30). `owner`, `peer` — учётные записи без адресов отправителя.
pub async fn identity_store(store: &dyn IdentityStore, owner: AccountScope, peer: AccountScope) {
    let ident = |id: &str, email: &str| Identity {
        id: id.into(),
        name: "Анна".into(),
        email: email.into(),
        reply_to: vec![IdentityAddress {
            name: Some("Продажи".into()),
            email: "sales@example.org".into(),
        }],
        bcc: Vec::new(),
        text_signature: "--\nАнна".into(),
        html_signature: "<p>Анна</p>".into(),
    };
    assert_eq!(store.state(owner).await.expect("state"), 0);
    assert!(store.list(owner).await.expect("list").is_empty());

    let s1 = store
        .put(owner, ident("work", "anna@example.org"))
        .await
        .expect("put");
    let s2 = store
        .put(owner, ident("home", "anna@example.net"))
        .await
        .expect("put");
    assert!(s1 > 0 && s2 > s1);
    let bad = Identity {
        email: "anna".into(),
        ..ident("bad", "x")
    };
    assert!(matches!(
        store.put(owner, bad).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(store.state(owner).await.expect("state"), s2);

    // Список по id; замена целиком.
    let ids: Vec<String> = store
        .list(owner)
        .await
        .expect("list")
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, ["home", "work"]);
    let renamed = Identity {
        name: "Анна Иванова".into(),
        reply_to: Vec::new(),
        ..ident("work", "anna@example.org")
    };
    let s3 = store.put(owner, renamed.clone()).await.expect("replace");
    assert!(s3 > s2);
    assert_eq!(store.get(owner, "work").await.expect("get"), Some(renamed));
    assert_eq!(store.get(owner, "none").await.expect("get"), None);

    // Учётные записи не видят адреса друг друга.
    assert!(store.list(peer).await.expect("peer").is_empty());
    assert_eq!(store.get(peer, "work").await.expect("peer"), None);
    assert_eq!(store.delete(peer, "work").await, Err(Error::NotFound));
    assert_eq!(store.state(peer).await.expect("peer"), 0);

    // Удаление.
    let s4 = store.delete(owner, "home").await.expect("delete");
    assert!(s4 > s3);
    assert_eq!(store.delete(owner, "home").await, Err(Error::NotFound));
    assert_eq!(store.list(owner).await.expect("list").len(), 1);
}

/// Набор проверок `SubmissionStore` (CTR-30): запись, отмена до отправки, фиксация, удаление, наступившие отправки.
/// `owner`, `peer` — учётные записи без отправок.
pub async fn submission_store(
    store: &dyn SubmissionStore,
    owner: AccountScope,
    peer: AccountScope,
) {
    // Целые секунды: PostgreSQL хранит микросекунды.
    let now = OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp())
        .expect("now");
    let sub = |id: &str, send_at: OffsetDateTime, undo: UndoStatus| EmailSubmission {
        id: id.into(),
        identity: "work".into(),
        email: MessageId::new(),
        mail_from: "anna@example.org".into(),
        rcpt_to: vec!["b@example.com".into(), "c@example.net".into()],
        send_at,
        undo_status: undo,
        delivery: Vec::new(),
        queue: None,
    };
    let later = now + Duration::from_secs(3600);
    assert_eq!(store.state(owner).await.expect("state"), 0);

    // Запись: сразу отправленная и две отложенные.
    let sent = EmailSubmission {
        delivery: vec![
            ("b@example.com".into(), SubmitStatus::Queued),
            ("c@example.net".into(), SubmitStatus::Delivered),
        ],
        queue: Some(QueueId::new()),
        ..sub("now", now, UndoStatus::Final)
    };
    let s1 = store.insert(owner, sent.clone()).await.expect("insert");
    let s2 = store
        .insert(owner, sub("later", later, UndoStatus::Pending))
        .await
        .expect("insert");
    let s3 = store
        .insert(owner, sub("soon", now, UndoStatus::Pending))
        .await
        .expect("insert");
    assert!(s1 > 0 && s2 > s1 && s3 > s2);
    assert_eq!(
        store
            .insert(owner, sub("now", now, UndoStatus::Final))
            .await,
        Err(Error::Conflict)
    );
    assert!(matches!(
        store
            .insert(owner, sub("x", now, UndoStatus::Canceled))
            .await,
        Err(Error::Invalid(_))
    ));
    let bad = EmailSubmission {
        rcpt_to: Vec::new(),
        ..sub("y", now, UndoStatus::Pending)
    };
    assert!(matches!(
        store.insert(owner, bad).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(store.get(owner, "now").await.expect("get"), Some(sent));

    // Список по (send_at, id); другая учётная запись не видит.
    let ids: Vec<String> = store
        .list(owner)
        .await
        .expect("list")
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, ["now", "soon", "later"]);
    assert!(store.list(peer).await.expect("peer").is_empty());
    assert_eq!(store.cancel(peer, "later").await, Err(Error::NotFound));

    // Наступившие: только Pending с send_at ≤ now.
    let mine = |v: Vec<(AccountScope, EmailSubmission)>| -> Vec<String> {
        v.into_iter()
            .filter(|(s, _)| *s == owner)
            .map(|(_, s)| s.id)
            .collect()
    };
    assert_eq!(mine(store.due(now, 10_000).await.expect("due")), ["soon"]);
    assert_eq!(
        mine(store.due(later, 10_000).await.expect("due")),
        ["soon", "later"]
    );

    // Отмена: Pending → Canceled, повтор — как есть, Final — Conflict.
    let canceled = store.cancel(owner, "later").await.expect("cancel");
    assert_eq!(canceled.undo_status, UndoStatus::Canceled);
    let state = store.state(owner).await.expect("state");
    assert!(state > s3);
    assert_eq!(store.cancel(owner, "later").await, Ok(canceled));
    assert_eq!(store.cancel(owner, "now").await, Err(Error::Conflict));
    assert_eq!(store.cancel(owner, "none").await, Err(Error::NotFound));
    assert_eq!(mine(store.due(later, 10_000).await.expect("due")), ["soon"]);

    // Фиксация: Pending → Final со статусами; отменённую — Conflict.
    let q = QueueId::new();
    let delivery = vec![
        ("b@example.com".to_owned(), SubmitStatus::Queued),
        (
            "c@example.net".to_owned(),
            SubmitStatus::Failed("550 5.1.1".into()),
        ),
    ];
    let s4 = store
        .finalize(owner, "soon", delivery.clone(), Some(q))
        .await
        .expect("finalize");
    assert!(s4 > state);
    let soon = store.get(owner, "soon").await.expect("get").expect("soon");
    assert_eq!(
        (soon.undo_status, soon.delivery, soon.queue),
        (UndoStatus::Final, delivery, Some(q))
    );
    assert_eq!(store.cancel(owner, "soon").await, Err(Error::Conflict));
    assert_eq!(
        store.finalize(owner, "later", Vec::new(), None).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        store.finalize(owner, "none", Vec::new(), None).await,
        Err(Error::NotFound)
    );
    assert!(mine(store.due(later, 10_000).await.expect("due")).is_empty());

    // Удаление.
    let s5 = store.delete(owner, "later").await.expect("delete");
    assert!(s5 > s4);
    assert_eq!(store.delete(owner, "later").await, Err(Error::NotFound));
    assert_eq!(store.list(owner).await.expect("list").len(), 2);
    assert_eq!(store.state(peer).await.expect("peer"), 0);
}

// ---------- BlobIndex: сжатие ----------

/// Набор проверок признака сжатия `BlobIndex` (`supports_compression`). Хэши набора — случайные:
/// чужие записи учёта не мешают.
pub async fn blob_compression(index: &dyn BlobIndex, tenant: TenantId) {
    let fresh = fresh_hash;
    let absent = async |hash: &BlobHash| match index
        .acquire(tenant, None, hash, 1)
        .await
        .expect("acquire")
    {
        Acquire::Absent { generation } => generation,
        other => panic!("ожидался Absent: {other:?}"),
    };
    let done = |c: Commit| match c {
        Commit::Done { generation } => generation,
        other => panic!("ожидался Done: {other:?}"),
    };
    let key = |hash: BlobHash, generation| BlobKey {
        tenant,
        hash,
        generation,
    };
    assert!(index.supports_compression());

    // Сжатый объект: признак в записи, повторная ссылка его не меняет.
    let zstd = fresh();
    let g = absent(&zstd).await;
    let g = done(
        index
            .commit_new_with(tenant, &zstd, g, 100, 1, Compression::Zstd)
            .await
            .expect("commit"),
    );
    assert_eq!(
        index.compression(&key(zstd, g)).await,
        Ok(Some(Compression::Zstd))
    );
    assert!(matches!(
        index.acquire(tenant, None, &zstd, 1).await,
        Ok(Acquire::Existing { generation }) if generation == g
    ));
    assert_eq!(
        index.compression(&key(zstd, g)).await,
        Ok(Some(Compression::Zstd))
    );

    // Параллельная загрузка без сжатия получает поколение существующей записи и её признак.
    let other = done(
        index
            .commit_new_with(tenant, &zstd, g, 100, 1, Compression::None)
            .await
            .expect("commit"),
    );
    assert_eq!(other, g);
    assert_eq!(
        index.compression(&key(zstd, g)).await,
        Ok(Some(Compression::Zstd))
    );

    // commit_new — без сжатия; нет записи или чужое поколение — None.
    let plain = fresh();
    let pg = absent(&plain).await;
    let pg = done(
        index
            .commit_new(tenant, &plain, pg, 10, 1)
            .await
            .expect("commit"),
    );
    assert_eq!(
        index.compression(&key(plain, pg)).await,
        Ok(Some(Compression::None))
    );
    assert_eq!(index.compression(&key(fresh(), g)).await, Ok(None));
    assert_eq!(index.compression(&key(zstd, g.next())).await, Ok(None));
}

// ---------- BlobIndex: срок GC и загрузки ----------

/// Случайный хэш: записи учёта других проверок не мешают.
fn fresh_hash() -> BlobHash {
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(AccountId::new().as_uuid().as_bytes());
    h[16..].copy_from_slice(AccountId::new().as_uuid().as_bytes());
    BlobHash(h)
}

/// Набор проверок `BlobIndex::next_gc_due` (CTR-38): срок — `MIN(zero_since) + window` кандидатов GC.
/// Учёт может быть общим (записи других проверок): набор сверяет срок с `gc_candidates`, а не с часами.
pub async fn blob_gc_due(index: &dyn BlobIndex, tenant: TenantId) {
    const W: Duration = Duration::from_secs(3600);
    let w = time::Duration::HOUR;
    let hash = fresh_hash();
    let generation = match index
        .acquire(tenant, None, &hash, 1)
        .await
        .expect("acquire")
    {
        Acquire::Absent { generation } => generation,
        other => panic!("ожидался Absent: {other:?}"),
    };
    let generation = match index
        .commit_new(tenant, &hash, generation, 1, 1)
        .await
        .expect("commit")
    {
        Commit::Done { generation } => generation,
        other => panic!("ожидался Done: {other:?}"),
    };
    index
        .release(tenant, &hash, generation, 1)
        .await
        .expect("release");

    // Есть кандидат — есть срок; раньше `due - W` обнулённых записей нет, чуть позже — есть.
    let due = index
        .next_gc_due(W)
        .await
        .expect("next_gc_due")
        .expect("есть запись без ссылок");
    assert!(
        index
            .gc_candidates(due - w, 1)
            .await
            .expect("gc")
            .is_empty(),
        "запись с zero_since раньше срока - W"
    );
    assert!(
        !index
            .gc_candidates(due - w + time::Duration::SECOND, 1)
            .await
            .expect("gc")
            .is_empty(),
        "нет записи с zero_since = срок - W"
    );
    // Срок сдвигается вместе с окном.
    let due2 = index
        .next_gc_due(2 * W)
        .await
        .expect("next_gc_due")
        .expect("срок");
    assert_eq!(due2, due + w);
}

/// Набор проверок владельцев загрузок `BlobIndex` (`mark_upload`/`upload_owned`/`prune_uploads`; CTR-38).
/// Моменты — текущие; `prune_uploads` — по всем организациям, поэтому число удалённых проверяется снизу.
pub async fn blob_uploads(index: &dyn BlobIndex, tenant: TenantId, foreign_tenant: TenantId) {
    let scope = AccountScope {
        tenant,
        account: AccountId::new(),
    };
    let other = AccountScope {
        tenant,
        account: AccountId::new(),
    };
    let key = |hash| BlobKey {
        tenant,
        hash,
        generation: Gen::INITIAL,
    };
    let (a, b) = (key(fresh_hash()), key(fresh_hash()));
    let now = OffsetDateTime::now_utc().replace_nanosecond(0).expect("ns");
    let s = time::Duration::SECOND;

    // Без отметки — не владелец; отметка — только для загрузившей учётной записи и до срока.
    assert_eq!(index.upload_owned(scope, &a, now).await, Ok(false));
    index
        .mark_upload(scope, &a, now + 10 * s)
        .await
        .expect("mark");
    assert_eq!(index.upload_owned(scope, &a, now).await, Ok(true));
    assert_eq!(index.upload_owned(other, &a, now).await, Ok(false));
    assert_eq!(index.upload_owned(scope, &b, now).await, Ok(false));
    assert_eq!(
        index.upload_owned(scope, &a, now + 10 * s).await,
        Ok(false),
        "срок истёк"
    );
    // Повторная отметка продлевает до большего срока, меньший срок не сокращает.
    index
        .mark_upload(scope, &a, now + 20 * s)
        .await
        .expect("extend");
    index
        .mark_upload(scope, &a, now + 5 * s)
        .await
        .expect("shorter");
    assert_eq!(index.upload_owned(scope, &a, now + 15 * s).await, Ok(true));
    // Объект другой организации — `Invalid`.
    let foreign = BlobKey {
        tenant: foreign_tenant,
        ..a
    };
    assert!(matches!(
        index.mark_upload(scope, &foreign, now + 10 * s).await,
        Err(Error::Invalid(_))
    ));

    // Очистка удаляет истёкшие на момент `now` отметки, действующие остаются.
    index
        .mark_upload(other, &b, now + 5 * s)
        .await
        .expect("mark b");
    let pruned = index
        .prune_uploads(now + 5 * s, usize::MAX)
        .await
        .expect("prune");
    assert!(pruned >= 1, "отметка b не удалена: {pruned}");
    assert_eq!(index.upload_owned(other, &b, now).await, Ok(false));
    assert_eq!(index.upload_owned(scope, &a, now).await, Ok(true));
    assert_eq!(index.prune_uploads(now + 30 * s, 0).await, Ok(0));
    assert_eq!(index.upload_owned(scope, &a, now).await, Ok(true));
}

// ---------- DeliveryLog ----------

/// Организации для [`delivery_log`].
#[derive(Debug, Clone, Copy)]
pub struct DeliveryFixture {
    /// Организация набора.
    pub tenant: TenantId,
    /// Другая организация.
    pub foreign_tenant: TenantId,
}

/// Набор проверок `DeliveryLog`: запись, поиск по отправителю, получателю, Message-ID и интервалу, страницы,
/// изоляция организаций. Адреса набора — с уникальной меткой: чужие записи не мешают. Моменты — текущие,
/// целые секунды; `purge` набор не проверяет (удаление по секциям — тест реализации).
pub async fn delivery_log(log: &dyn DeliveryLog, fx: &DeliveryFixture) {
    let tag = AccountId::new().to_string();
    let addr = |who: &str| format!("{who}-{tag}@example.org");
    let now = OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("time");
    let at = |s: i64| now - time::Duration::seconds(100 - s);
    let node = NodeId::new("suite-node").expect("node");
    let event = |s: i64, kind, from: &str, to: &[&str], msg: &str| DeliveryEvent {
        tenant: fx.tenant,
        at: at(s),
        node: node.clone(),
        kind,
        sender: addr(from),
        recipients: to.iter().map(|r| addr(r)).collect(),
        message_id: Some(format!("{msg}.{tag}@example.org")),
        queue_id: None,
        decision: DeliveryDecision::Accept,
        reason: None,
    };
    use DeliveryEventKind as K;
    let received = event(0, K::Receive, "alice", &["bob", "carol"], "m1");
    let delivered = event(1, K::Deliver, "alice", &["bob", "carol"], "m1");
    let rejected = DeliveryEvent {
        decision: DeliveryDecision::Reject,
        reason: Some("550 5.7.1 spam".into()),
        message_id: None,
        ..event(2, K::Reject, "mallory", &["bob"], "m2")
    };
    let relayed = DeliveryEvent {
        queue_id: Some(QueueId::new()),
        ..event(3, K::Relay, "bob", &["dave"], "m3")
    };
    let dsn = DeliveryEvent {
        sender: String::new(),
        ..event(4, K::Dsn, "x", &["bob"], "m3")
    };
    // Записи в другом порядке: поиск упорядочивает по моменту.
    log.record(relayed.clone()).await.expect("record");
    log.record_batch(vec![received.clone(), delivered.clone(), rejected.clone()])
        .await
        .expect("batch");
    log.record(dsn.clone()).await.expect("record");
    log.record(DeliveryEvent {
        tenant: fx.foreign_tenant,
        ..received.clone()
    })
    .await
    .expect("foreign");

    let search = async |q: DeliveryQuery| log.search(fx.tenant, &q).await.expect("search");
    let mut by = DeliveryQuery::all(100);
    by.recipient = Some(addr("BOB"));
    assert_eq!(
        search(by.clone()).await,
        DeliveryPage {
            events: vec![
                received.clone(),
                delivered.clone(),
                rejected.clone(),
                dsn.clone()
            ],
            next: None,
        },
        "получатель без учёта регистра, по моменту"
    );

    let mut q = DeliveryQuery::all(100);
    q.sender = Some(addr("alice").to_uppercase());
    assert_eq!(
        search(q).await.events,
        vec![received.clone(), delivered.clone()]
    );
    let mut q = DeliveryQuery::all(100);
    q.message_id = received.message_id.clone();
    assert_eq!(
        search(q).await.events,
        vec![received.clone(), delivered.clone()]
    );
    let mut q = DeliveryQuery::all(100);
    q.message_id = relayed.message_id.clone();
    q.sender = Some(addr("bob"));
    assert_eq!(search(q).await.events, vec![relayed.clone()]);

    // Интервал [since, until).
    let mut q = by.clone();
    q.since = Some(at(1));
    q.until = Some(at(4));
    assert_eq!(
        search(q).await.events,
        vec![delivered.clone(), rejected.clone()]
    );

    // Страницы по limit.
    let mut q = by.clone();
    q.limit = 3;
    let first = search(q.clone()).await;
    assert_eq!(
        first.events,
        vec![received.clone(), delivered.clone(), rejected.clone()]
    );
    q.after = Some(first.next.expect("next page"));
    assert_eq!(
        search(q).await,
        DeliveryPage {
            events: vec![dsn.clone()],
            next: None,
        }
    );

    // Некорректный запрос — Invalid; другая организация видит только своё.
    let mut q = by.clone();
    q.limit = 0;
    assert!(matches!(
        log.search(fx.tenant, &q).await,
        Err(Error::Invalid(_))
    ));
    let mut q = DeliveryQuery::all(100);
    q.message_id = received.message_id.clone();
    assert_eq!(
        log.search(fx.foreign_tenant, &q)
            .await
            .expect("foreign")
            .events
            .len(),
        1
    );
}

// ---------- GroupStore ----------

/// Учётные записи для [`group_store`]; создаёт тест до запуска набора.
#[derive(Debug, Clone, Copy)]
pub struct GroupFixture {
    /// Организация набора.
    pub tenant: TenantId,
    /// Учётные записи организации.
    pub accounts: [AccountId; 4],
    /// Другая организация.
    pub foreign_tenant: TenantId,
}

/// Набор проверок `GroupStore`: группы, вложенность и циклы, ограничения отправителей, очередь модерации,
/// изоляция организаций. Адреса и `external_id` набора — с уникальной меткой.
pub async fn group_store(store: &dyn GroupStore, fx: &GroupFixture) {
    let tag = AccountId::new().to_string();
    let t = fx.tenant;
    let [a, b, c, d] = fx.accounts;
    let group = |name: &str, members: Vec<GroupMember>| DistributionGroup {
        id: GroupId::new(),
        tenant: t,
        name: format!("{name} {tag}"),
        address: Some(format!("{name}-{tag}@example.org")),
        external_id: Some(format!("{name}-{tag}")),
        members,
        senders: SenderRestriction::Anyone,
        moderators: Vec::new(),
        hidden_from_gal: false,
    };
    let sorted = |mut v: Vec<AccountId>| {
        v.sort();
        v
    };

    // Вложенность с циклом: all → (a, team), team → (b, inner, all), inner → (c, team); d — вне групп.
    let mut all = group("all", vec![GroupMember::Account(a)]);
    let mut team = group("team", vec![GroupMember::Account(b)]);
    let inner = DistributionGroup {
        address: None,
        ..group(
            "inner",
            vec![GroupMember::Account(c), GroupMember::Group(team.id)],
        )
    };
    all.members.push(GroupMember::Group(team.id));
    team.members.extend([
        GroupMember::Group(inner.id),
        GroupMember::Group(all.id),
        GroupMember::Group(GroupId::new()), // неизвестная группа
        GroupMember::Account(b),            // повтор
    ]);
    for g in [&all, &team, &inner] {
        store.put(g.clone()).await.expect("put");
    }
    assert_eq!(store.expand(t, all.id).await, Ok(sorted(vec![a, b, c])));
    assert_eq!(store.expand(t, inner.id).await, Ok(sorted(vec![a, b, c])));
    assert_eq!(store.expand(t, GroupId::new()).await, Err(Error::NotFound));

    // Поиск: по id, адресу без учёта регистра, external_id; список организации.
    assert_eq!(store.get(t, all.id).await, Ok(Some(all.clone())));
    let upper = all.address.clone().expect("address").to_uppercase();
    assert_eq!(store.by_address(t, &upper).await, Ok(Some(all.clone())));
    assert_eq!(
        store.by_external_id(t, &format!("team-{tag}")).await,
        Ok(Some(team.clone()))
    );
    let mine: Vec<_> = store
        .list(t)
        .await
        .expect("list")
        .into_iter()
        .filter(|g| g.name.ends_with(&tag))
        .collect();
    let mut expected = vec![all.clone(), team.clone(), inner.clone()];
    expected.sort_by_key(|g| g.id);
    assert_eq!(mine, expected);

    // Замена целиком; занятый адрес или external_id — Conflict; некорректное — Invalid.
    let hidden = DistributionGroup {
        hidden_from_gal: true,
        senders: SenderRestriction::Internal,
        ..team.clone()
    };
    store.put(hidden.clone()).await.expect("replace");
    assert_eq!(store.get(t, team.id).await, Ok(Some(hidden.clone())));
    let clash = DistributionGroup {
        address: Some(upper.clone()),
        ..group("clash", Vec::new())
    };
    assert_eq!(store.put(clash).await, Err(Error::Conflict));
    let clash = DistributionGroup {
        external_id: all.external_id.clone(),
        ..group("clash2", Vec::new())
    };
    assert_eq!(store.put(clash).await, Err(Error::Conflict));
    let bad = DistributionGroup {
        name: String::new(),
        ..group("bad", Vec::new())
    };
    assert!(matches!(store.put(bad).await, Err(Error::Invalid(_))));

    // Ограничения отправителей.
    assert_eq!(store.sender_allowed(t, &all, None).await, Ok(true));
    assert_eq!(store.sender_allowed(t, &hidden, None).await, Ok(false));
    assert_eq!(store.sender_allowed(t, &hidden, Some(d)).await, Ok(true));
    let closed = DistributionGroup {
        senders: SenderRestriction::Only {
            accounts: vec![d],
            groups: vec![inner.id],
        },
        moderators: vec![a],
        ..group("closed", Vec::new())
    };
    store.put(closed.clone()).await.expect("put");
    assert_eq!(store.sender_allowed(t, &closed, Some(d)).await, Ok(true));
    assert_eq!(store.sender_allowed(t, &closed, Some(c)).await, Ok(true));
    assert_eq!(store.sender_allowed(t, &closed, None).await, Ok(false));

    // Очередь модерации: по моменту, по группе; take_held — один раз.
    assert!(closed.requires_moderation(Some(d)));
    let base = OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("time");
    let held = |s: i64, group: GroupId| HeldMessage {
        id: HeldId::new(),
        group: Some(group),
        source: HoldSource::Group,
        moderators: Vec::new(),
        recipients: Vec::new(),
        sender: format!("d-{tag}@example.org"),
        sender_account: Some(d),
        message_id: Some(format!("{s}.{tag}@example.org")),
        blob: BlobKey {
            tenant: t,
            hash: BlobHash([7; 32]),
            generation: Gen::INITIAL,
        },
        size: 1000,
        held_at: base + time::Duration::seconds(s),
    };
    let (h2, h1, h3) = (held(2, closed.id), held(1, closed.id), held(3, all.id));
    for h in [&h2, &h1, &h3] {
        store.hold(t, h.clone()).await.expect("hold");
    }
    assert_eq!(store.hold(t, h1.clone()).await, Err(Error::Conflict));
    assert_eq!(
        store.hold(t, held(4, GroupId::new())).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.held(t, Some(closed.id), 10).await,
        Ok(vec![h1.clone(), h2.clone()])
    );
    assert_eq!(
        store.held(t, Some(closed.id), 1).await,
        Ok(vec![h1.clone()])
    );
    assert_eq!(store.take_held(t, h1.id).await, Ok(h1.clone()));
    assert_eq!(store.take_held(t, h1.id).await, Err(Error::NotFound));

    // Другая организация не видит группы и очередь.
    let f = fx.foreign_tenant;
    assert_eq!(store.get(f, all.id).await, Ok(None));
    assert_eq!(store.by_address(f, &upper).await, Ok(None));
    assert_eq!(store.expand(f, all.id).await, Err(Error::NotFound));
    assert_eq!(store.held(f, None, 10).await, Ok(Vec::new()));
    assert_eq!(store.take_held(f, h2.id).await, Err(Error::NotFound));
    assert_eq!(store.remove(f, closed.id).await, Ok(Vec::new()));
    assert_eq!(store.get(t, closed.id).await, Ok(Some(closed.clone())));

    // Удаление возвращает очередь группы; вложенная ссылка становится неизвестной; повтор — не ошибка.
    assert_eq!(store.remove(t, closed.id).await, Ok(vec![h2]));
    assert_eq!(store.remove(t, closed.id).await, Ok(Vec::new()));
    assert_eq!(store.get(t, closed.id).await, Ok(None));
    store.remove(t, inner.id).await.expect("remove");
    assert_eq!(store.expand(t, all.id).await, Ok(sorted(vec![a, b])));
    assert_eq!(store.held(t, Some(all.id), 10).await, Ok(vec![h3]));
}

// ---------- AccountStore: external_id ----------

/// Данные набора [`account_external_id`]: домен организации `tenant` (`domain_name` — его имя для адресов входа)
/// и другая организация.
#[derive(Debug, Clone)]
pub struct ExternalIdFixture {
    /// Организация.
    pub tenant: TenantId,
    /// Домен организации.
    pub domain: DomainId,
    /// Имя домена.
    pub domain_name: String,
    /// Другая организация.
    pub foreign_tenant: TenantId,
}

/// Набор проверок `AccountStore::by_external_id` (CTR-32): поиск по идентификатору каталога, переименование входа
/// сохраняет учётную запись и её членство в группе, изоляция организаций.
pub async fn account_external_id(
    accounts: &dyn AccountStore,
    groups: &dyn GroupStore,
    fx: &ExternalIdFixture,
) {
    let run = AccountId::new();
    let ext = format!("ext-{run}");
    let login = |n: &str| format!("{n}-{run}@{}", fx.domain_name);
    let upsert = |login: String| AccountUpsert {
        domain: fx.domain,
        login,
        display_name: "Анна".into(),
        external_id: Some(ext.clone()),
        enabled: true,
    };
    assert_eq!(
        accounts
            .by_external_id(fx.tenant, &ext)
            .await
            .expect("none"),
        None
    );
    let a = accounts
        .upsert(fx.tenant, upsert(login("anna")))
        .await
        .expect("upsert");
    assert_eq!(
        accounts
            .by_external_id(fx.tenant, &ext)
            .await
            .expect("by ext"),
        Some(a.clone())
    );
    assert_eq!(
        accounts
            .by_external_id(fx.tenant, &ext.to_uppercase())
            .await
            .expect("case"),
        None,
        "точное совпадение"
    );
    assert_eq!(
        accounts
            .by_external_id(fx.foreign_tenant, &ext)
            .await
            .expect("foreign"),
        None
    );

    // Группа с участником; переименование входа в каталоге — та же учётная запись, членство сохраняется.
    let group = DistributionGroup {
        id: GroupId::new(),
        tenant: fx.tenant,
        name: "Отдел".into(),
        address: None,
        external_id: None,
        members: vec![GroupMember::Account(a.id)],
        senders: SenderRestriction::Anyone,
        moderators: Vec::new(),
        hidden_from_gal: false,
    };
    groups.put(group.clone()).await.expect("group");
    let renamed = accounts
        .upsert(fx.tenant, upsert(login("anna.ivanova")))
        .await
        .expect("rename");
    assert_eq!(renamed.id, a.id);
    assert_eq!(renamed.login, login("anna.ivanova"));
    let found = accounts
        .by_external_id(fx.tenant, &ext)
        .await
        .expect("by ext")
        .expect("found");
    assert_eq!((found.id, found.login), (a.id, login("anna.ivanova")));
    assert_eq!(
        accounts
            .by_login(fx.tenant, &login("anna"))
            .await
            .expect("old"),
        None
    );
    assert_eq!(
        groups.expand(fx.tenant, group.id).await.expect("expand"),
        vec![a.id]
    );
}

// ---------- AccountStore: list ----------

/// Данные набора [`account_list`]: организация `tenant` без учётных записей и её домен (`domain_name` — имя для
/// адресов входа), другая организация `foreign_tenant` и её домен.
#[derive(Debug, Clone)]
pub struct AccountListFixture {
    /// Организация без учётных записей.
    pub tenant: TenantId,
    /// Домен организации.
    pub domain: DomainId,
    /// Имя домена.
    pub domain_name: String,
    /// Другая организация.
    pub foreign_tenant: TenantId,
    /// Домен другой организации.
    pub foreign_domain: DomainId,
}

/// Набор проверок `AccountStore::list` (CTR-44, META-45): побайтный порядок имени входа в нижнем регистре, страницы и
/// курсор `after` (строго больше, в том числе несуществующего имени), префикс (обычные знаки, не шаблон `LIKE`),
/// регистр, `limit = 0`, изоляция организаций. Имена различаются знаками `.`, `-`, `_` и не-ASCII: побайтно
/// `-` < `.` < `@` < `_` < буквы < не-ASCII, правила сортировки базы (en_US) дали бы иной порядок. Не-ASCII — буквы
/// с простым соответствием регистров (`É`, `Ю`): `str::to_lowercase` и `lower` PostgreSQL с UTF-8 `LC_CTYPE`
/// совпадают; знаки с особыми правилами (`İ`, `Σ`) в набор не входят.
pub async fn account_list(accounts: &dyn AccountStore, fx: &AccountListFixture) {
    let login = |n: &str| format!("{n}@{}", fx.domain_name);
    let lower = |n: &str| login(n).to_lowercase();
    let list = |tenant: TenantId, prefix: &'static str, after: Option<String>, limit: usize| async move {
        accounts
            .list(tenant, prefix, after.as_deref(), limit)
            .await
            .expect("list")
    };
    assert_eq!(list(fx.tenant, "", None, 10).await, Vec::new());

    let mut created = std::collections::BTreeMap::new();
    for n in [
        "boris", "Anton", "dina", "anna", "BELLA", "e_va", "eva", "e.va", "Юрий", "e", "Émile",
        "e-va",
    ] {
        let acc = accounts
            .upsert(
                fx.tenant,
                AccountUpsert {
                    domain: fx.domain,
                    login: login(n),
                    display_name: n.into(),
                    external_id: None,
                    enabled: true,
                },
            )
            .await
            .expect("upsert");
        created.insert(n, acc);
    }
    let foreign = accounts
        .upsert(
            fx.foreign_tenant,
            AccountUpsert {
                domain: fx.foreign_domain,
                login: login("aaron"),
                display_name: "aaron".into(),
                external_id: None,
                enabled: true,
            },
        )
        .await
        .expect("upsert foreign");
    let accs =
        |names: &[&str]| -> Vec<Account> { names.iter().map(|n| created[n].clone()).collect() };

    // Весь список: побайтно по возрастанию имени в нижнем регистре, без чужой организации.
    let all = [
        "anna", "Anton", "BELLA", "boris", "dina", "e-va", "e.va", "e", "e_va", "eva", "Émile",
        "Юрий",
    ];
    assert_eq!(list(fx.tenant, "", None, 20).await, accs(&all));
    // Страницы по курсору последней записи; после последней — пусто.
    assert_eq!(list(fx.tenant, "", None, 2).await, accs(&["anna", "Anton"]));
    assert_eq!(
        list(fx.tenant, "", Some(lower("Anton")), 2).await,
        accs(&["BELLA", "boris"])
    );
    assert_eq!(
        list(fx.tenant, "", Some(lower("boris")), 2).await,
        accs(&["dina", "e-va"])
    );
    // Страницы по всему списку курсором последней записи дают весь список.
    let mut paged = Vec::new();
    let mut after = None;
    loop {
        let page = list(fx.tenant, "", after, 2).await;
        let Some(last) = page.last() else { break };
        after = Some(last.login.to_lowercase());
        paged.extend(page);
    }
    assert_eq!(paged, accs(&all));
    assert_eq!(
        list(fx.tenant, "", Some(lower("Юрий")), 2).await,
        Vec::new()
    );
    // Курсор — не обязательно существующее имя; строго больше.
    assert_eq!(
        list(fx.tenant, "", Some("b".into()), 20).await,
        accs(&all[2..])
    );
    assert_eq!(
        list(fx.tenant, "", Some("e/".into()), 20).await,
        accs(&["e", "e_va", "eva", "Émile", "Юрий"])
    );
    // Префикс в нижнем регистре находит имена в любом регистре; с курсором.
    assert_eq!(
        list(fx.tenant, "b", None, 10).await,
        accs(&["BELLA", "boris"])
    );
    assert_eq!(
        list(fx.tenant, "an", None, 10).await,
        accs(&["anna", "Anton"])
    );
    assert_eq!(
        list(fx.tenant, "an", Some(lower("anna")), 10).await,
        accs(&["Anton"])
    );
    // Префикс со знаками: `e` — без `é`; `_` и `%` — обычные знаки; не-ASCII в любом регистре.
    assert_eq!(
        list(fx.tenant, "e", None, 10).await,
        accs(&["e-va", "e.va", "e", "e_va", "eva"])
    );
    assert_eq!(
        list(fx.tenant, "e", Some(lower("e.va")), 10).await,
        accs(&["e", "e_va", "eva"])
    );
    assert_eq!(list(fx.tenant, "e_", None, 10).await, accs(&["e_va"]));
    assert_eq!(list(fx.tenant, "e.", None, 10).await, accs(&["e.va"]));
    assert_eq!(list(fx.tenant, "e-", None, 10).await, accs(&["e-va"]));
    assert_eq!(list(fx.tenant, "e@", None, 10).await, accs(&["e"]));
    assert_eq!(list(fx.tenant, "é", None, 10).await, accs(&["Émile"]));
    assert_eq!(list(fx.tenant, "юр", None, 10).await, accs(&["Юрий"]));
    for none in ["x", "%", "e%", "_"] {
        assert_eq!(list(fx.tenant, none, None, 10).await, Vec::new(), "{none}");
    }
    assert_eq!(list(fx.tenant, "", None, 0).await, Vec::new());
    // Другая организация видит только своё.
    assert_eq!(list(fx.foreign_tenant, "", None, 10).await, vec![foreign]);
}

// ---------- Moderation ----------

/// Данные набора [`moderation`]: модерируемая группа организации `tenant` и письма её очереди, уже забранные
/// ([`GroupStore::take_held`]): `approve` — для одобрения, `decline` — для отказа (их объекты существуют).
#[derive(Debug, Clone)]
pub struct ModerationFixture {
    /// Организация.
    pub tenant: TenantId,
    /// Группа писем.
    pub group: DistributionGroup,
    /// Письмо для одобрения.
    pub approve: HeldMessage,
    /// Письмо для отказа.
    pub decline: HeldMessage,
}

/// Набор проверок `Moderation` (CTR-31): чужая группа и чужая организация — `Invalid`, недопустимая причина —
/// `Invalid`, одобрение и отказ проходят. Что доставлено и какое уведомление ушло — проверки реализации.
pub async fn moderation(m: &dyn Moderation, fx: &ModerationFixture) {
    let other = DistributionGroup {
        id: GroupId::new(),
        ..fx.group.clone()
    };
    assert!(matches!(
        m.deliver(fx.tenant, Some(&other), &fx.approve).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        m.deliver(TenantId::new(), Some(&fx.group), &fx.approve)
            .await,
        Err(Error::Invalid(_))
    ));
    for bad in ["", "a\nb"] {
        assert!(matches!(
            m.reject(fx.tenant, &fx.decline, bad).await,
            Err(Error::Invalid(_))
        ));
    }
    assert!(matches!(
        m.deliver(fx.tenant, None, &fx.approve).await,
        Err(Error::Invalid(_))
    ));
    m.deliver(fx.tenant, Some(&fx.group), &fx.approve)
        .await
        .expect("deliver");
    m.reject(fx.tenant, &fx.decline, "off-topic")
        .await
        .expect("reject");
}

// ---------- Trash: trash.purge ----------

/// Запись правила администратора `trash.purge` уровня организации для [`trash_purge`]
/// (запись политик уровней — вне контрактов до CTR-25: in-memory — `MemPolicies`, PostgreSQL — `PgMeta::set_policy`).
/// Остальные поля политики уровня сохраняются.
#[async_trait]
pub trait TrashPurgeAdmin: Send + Sync {
    /// Задаёт правило организации; `None` — снять.
    async fn set_tenant_rule(&self, tenant: TenantId, rule: Option<TrashPurgeRule>);
}

/// Учётная запись для [`trash_purge`]: создаёт тест до запуска набора (системные папки). Набор меняет правило
/// `trash.purge` её организации и в конце снимает его: другие тесты этой организации параллельно не запускать.
#[derive(Debug, Clone, Copy)]
pub struct TrashPurgeFixture {
    /// Учётная запись.
    pub scope: AccountScope,
}

/// Точка принятия решений, отвечающая ошибкой (модуль недоступен).
struct FailingHook;

#[async_trait]
impl DecisionHook for FailingHook {
    fn name(&self) -> &str {
        "failing"
    }

    async fn before_final_delete(&self, _op: &FinalDelete) -> tp_contracts::Result<Decision> {
        Err(Error::internal("hook unavailable"))
    }
}

async fn role_folder(
    mailboxes: &dyn MailboxStore,
    scope: AccountScope,
    role: SpecialUse,
) -> MailboxId {
    mailboxes
        .list(scope)
        .await
        .expect("mailboxes")
        .into_iter()
        .find(|m| m.role == Some(role))
        .expect("system folder")
        .id
}

/// Момент приёма писем набора (целые секунды).
fn purge_received_at() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts")
}

async fn put_message(
    messages: &dyn MessageStore,
    scope: AccountScope,
    mailbox: MailboxId,
    size: u64,
) -> MessageId {
    let mut hash = [0u8; 32];
    hash[..16].copy_from_slice(MessageId::new().as_uuid().as_bytes());
    let msg = NewMessage {
        skeleton: BlobKey {
            tenant: scope.tenant,
            hash: BlobHash(hash),
            generation: Gen::INITIAL,
        },
        parts: Vec::new(),
        size,
        received_at: purge_received_at(),
        flags: Default::default(),
        meta: Default::default(),
    };
    messages
        .insert(scope, mailbox, msg)
        .await
        .expect("insert")
        .id
}

/// Письма элементов корзины по возрастанию идентификатора.
fn trash_ids(items: Vec<TrashItem>) -> Vec<MessageId> {
    let mut ids: Vec<_> = items
        .into_iter()
        .filter_map(|i| match i.object {
            TrashObject::Message(id) => Some(id),
            _ => None,
        })
        .collect();
    ids.sort_unstable();
    ids
}

fn sorted_ids(mut ids: Vec<MessageId>) -> Vec<MessageId> {
    ids.sort_unstable();
    ids
}

/// Набор проверок «очистить корзину» (`TrashStore::purge`, `list_hidden`, `restore`) и настройки `trash.purge`
/// (`PolicyResolver::trash_purge`, `set_user_trash_purge`): `hide` скрывает письмо (нет в `MessageStore`, в счётчиках
/// корзины, в квоте), администратор его восстанавливает; `erase` стирает; Legal Hold и ошибка точки принятия решений
/// не дают стереть; пользователь не меняет значение без `user_can_choose`, значение администратора приоритетнее.
pub async fn trash_purge(
    trash: &dyn TrashStore,
    messages: &dyn MessageStore,
    mailboxes: &dyn MailboxStore,
    quota: &dyn Quota,
    policies: &dyn PolicyResolver,
    admin: &dyn TrashPurgeAdmin,
    fx: &TrashPurgeFixture,
) {
    let s = fx.scope;
    let none = DecisionHooks::default();
    let inbox = role_folder(mailboxes, s, SpecialUse::Inbox).await;
    let bin = role_folder(mailboxes, s, SpecialUse::Trash).await;
    let rule = |mode, user_can_choose| {
        Some(TrashPurgeRule {
            mode,
            user_can_choose,
        })
    };

    admin.set_tenant_rule(s.tenant, None).await;
    policies.set_user_trash_purge(s, None).await.expect("reset");
    assert_eq!(
        policies.trash_purge(s).await,
        Ok(TrashPurgeSetting::default())
    );
    let base = quota.usage(s).await.expect("usage");

    // По умолчанию — hide: письмо пропадает у пользователя и из квоты, администратор видит его скрытым.
    let a = put_message(messages, s, inbox, 100).await;
    let b = put_message(messages, s, inbox, 200).await;
    let c = put_message(messages, s, inbox, 50).await;
    messages.to_trash(s, &[a, b]).await.expect("to trash");
    let out = trash
        .purge(s, &[a, c, MessageId::new(), a], &none)
        .await
        .expect("purge");
    assert_eq!(out.hidden, [a], "вне корзины и повтор — пропуск");
    assert!(out.erased.is_empty());
    assert_eq!(messages.get(s, a).await, Ok(None));
    assert_eq!(
        messages.get(s, c).await.expect("get").map(|m| m.mailbox),
        Some(inbox)
    );
    let left: Vec<_> = messages
        .list(s, bin, MessageQuery::All, 1000)
        .await
        .expect("list trash")
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(left, [b]);
    assert_eq!(mailboxes.status(s, bin).await.expect("status").messages, 1);
    assert_eq!(trash_ids(trash.list(s, 0, 100).await.expect("list")), [b]);
    let hidden = trash.list_hidden(s, 0, 100).await.expect("hidden");
    assert_eq!(trash_ids(hidden.clone()), [a]);
    assert_eq!((hidden[0].origin, hidden[0].size), (inbox, 100));
    let used = quota.usage(s).await.expect("usage");
    assert_eq!(
        (used.used_bytes, used.messages),
        (base.used_bytes + 250, base.messages + 2),
        "скрытое не в квоте"
    );
    let again = trash.purge(s, &[a], &none).await.expect("again");
    assert!(
        again.hidden.is_empty() && again.erased.is_empty(),
        "уже скрытое — пропуск"
    );
    assert!(
        messages.restore(s, &[a]).await.is_err(),
        "пользователь скрытое не восстанавливает"
    );

    // Администратор восстанавливает скрытое в исходную папку с прежним идентификатором.
    let Restored::Message { mailbox, inserted } = trash
        .restore(s, TrashObject::Message(a))
        .await
        .expect("restore hidden")
    else {
        panic!("message expected");
    };
    assert_eq!((mailbox, inserted.id), (inbox, a));
    assert_eq!(
        messages.get(s, a).await.expect("get").map(|m| m.mailbox),
        Some(inbox)
    );
    assert!(
        trash
            .list_hidden(s, 0, 100)
            .await
            .expect("hidden")
            .is_empty()
    );
    assert_eq!(
        quota.usage(s).await.expect("usage").used_bytes,
        base.used_bytes + 350
    );

    // erase администратора: стирает сразу, восстановить нечего.
    admin
        .set_tenant_rule(s.tenant, rule(TrashPurge::Erase, false))
        .await;
    assert_eq!(
        policies.trash_purge(s).await.expect("setting").effective(),
        TrashPurge::Erase
    );
    messages.to_trash(s, &[a]).await.expect("to trash");
    let out = trash.purge(s, &[a], &none).await.expect("erase");
    assert_eq!(out.erased, [a]);
    assert!(out.hidden.is_empty());
    assert_eq!(messages.get(s, a).await, Ok(None));
    assert!(
        trash
            .list_hidden(s, 0, 100)
            .await
            .expect("hidden")
            .is_empty()
    );
    assert_eq!(
        trash.restore(s, TrashObject::Message(a)).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        quota.usage(s).await.expect("usage").used_bytes,
        base.used_bytes + 250
    );

    // Legal Hold (запрет точки принятия решений) и её ошибка: erase работает как hide.
    let hold = RecordingDecisionHook::new("legal-hold");
    hold.set_final_delete(Decision::Deny("legal hold".into()));
    let held = DecisionHooks::new(vec![Arc::new(hold.clone())]);
    let out = trash.purge(s, &[b], &held).await.expect("hold");
    assert_eq!(out.hidden, [b]);
    assert!(out.erased.is_empty());
    assert_eq!(
        hold.calls(),
        [HookCall::FinalDelete(FinalDelete {
            scope: s,
            mailbox: inbox,
            message: b,
            received_at: purge_received_at(),
        })]
    );
    messages.to_trash(s, &[c]).await.expect("to trash");
    let failing = DecisionHooks::new(vec![Arc::new(FailingHook)]);
    let out = trash.purge(s, &[c], &failing).await.expect("hook error");
    assert_eq!(out.hidden, [c]);
    assert_eq!(
        trash_ids(trash.list_hidden(s, 0, 100).await.expect("hidden")),
        sorted_ids(vec![b, c])
    );

    // Пользователь без user_can_choose значение не меняет; сброс разрешён.
    assert_eq!(
        policies
            .set_user_trash_purge(s, Some(TrashPurge::Hide))
            .await,
        Err(Error::Forbidden)
    );
    assert_eq!(policies.set_user_trash_purge(s, None).await, Ok(()));
    assert_eq!(
        policies.trash_purge(s).await.expect("setting").effective(),
        TrashPurge::Erase
    );

    // С user_can_choose действует выбор пользователя; без разрешения — значение администратора.
    admin
        .set_tenant_rule(s.tenant, rule(TrashPurge::Hide, true))
        .await;
    policies
        .set_user_trash_purge(s, Some(TrashPurge::Erase))
        .await
        .expect("choose");
    let setting = policies.trash_purge(s).await.expect("setting");
    assert_eq!(
        (setting.user, setting.effective()),
        (Some(TrashPurge::Erase), TrashPurge::Erase)
    );
    let d = put_message(messages, s, inbox, 10).await;
    messages.to_trash(s, &[d]).await.expect("to trash");
    let out = trash.purge(s, &[d], &none).await.expect("user erase");
    assert_eq!(out.erased, [d]);
    admin
        .set_tenant_rule(s.tenant, rule(TrashPurge::Hide, false))
        .await;
    assert_eq!(
        policies.trash_purge(s).await.expect("setting").effective(),
        TrashPurge::Hide
    );
    admin
        .set_tenant_rule(s.tenant, rule(TrashPurge::Erase, true))
        .await;
    policies
        .set_user_trash_purge(s, Some(TrashPurge::Hide))
        .await
        .expect("choose hide");
    assert_eq!(
        policies.trash_purge(s).await.expect("setting").effective(),
        TrashPurge::Hide
    );

    // Другая учётная запись не видит и не восстанавливает скрытое.
    let other = AccountScope {
        tenant: s.tenant,
        account: AccountId::new(),
    };
    assert!(
        trash
            .list_hidden(other, 0, 100)
            .await
            .unwrap_or_default()
            .is_empty()
    );
    assert_eq!(
        trash.restore(other, TrashObject::Message(b)).await,
        Err(Error::NotFound)
    );

    policies.set_user_trash_purge(s, None).await.expect("reset");
    admin.set_tenant_rule(s.tenant, None).await;
}

// ---------- TransportStore ----------

/// Набор проверок `TransportStore` (CTR-24): версии снимков (устаревшая — `Conflict`), ключи записей, проверка полей,
/// изоляция снимков организаций. Пустой кластер не требуется: домены и источники набора — с уникальными именами,
/// набор удаляет их в конце. Набор пишет уровень кластера — параллельные писатели кластерных настроек дадут `Conflict`.
pub async fn transport_store(store: &dyn TransportStore) {
    let (tenant, other) = (TenantId::new(), TenantId::new());
    let tag = tenant.to_string().replace('-', "");
    let tag = &tag[tag.len() - 12..];
    let name = |n: &str| format!("{n}-{tag}.example");
    let domain = AcceptedDomain {
        id: DomainId::new(),
        tenant,
        name: name("a"),
        kind: DomainKind::Authoritative,
    };

    // Домены: версия растёт, устаревшая — Conflict, без изменения снимка.
    let v0 = store.cluster().await.expect("cluster").version;
    let v1 = store.put_domain(v0, &domain).await.expect("put domain");
    assert!(v1 > v0);
    let snap = store.cluster().await.expect("cluster");
    assert_eq!(snap.version, v1);
    assert_eq!(snap.domain(&domain.name), Some(&domain));
    let relay = AcceptedDomain {
        kind: DomainKind::InternalRelay {
            next_hop: "old-mail.example:25".into(),
        },
        ..domain.clone()
    };
    assert_eq!(store.put_domain(v0, &relay).await, Err(Error::Conflict));
    assert_eq!(
        store.cluster().await.expect("cluster").domain(&domain.name),
        Some(&domain)
    );
    let v2 = store.put_domain(v1, &relay).await.expect("replace domain");
    assert!(v2 > v1);
    assert_eq!(
        store.cluster().await.expect("cluster").domain(&domain.name),
        Some(&relay)
    );

    // Имя занято другим доменом — Conflict; смена организации — Invalid; неканоничное имя — Invalid.
    let twin = AcceptedDomain {
        id: DomainId::new(),
        ..domain.clone()
    };
    assert_eq!(store.put_domain(v2, &twin).await, Err(Error::Conflict));
    let moved = AcceptedDomain {
        tenant: other,
        ..domain.clone()
    };
    assert!(matches!(
        store.put_domain(v2, &moved).await,
        Err(Error::Invalid(_))
    ));
    let upper = AcceptedDomain {
        id: DomainId::new(),
        name: name("B"),
        ..domain.clone()
    };
    assert!(matches!(
        store.put_domain(v2, &upper).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(store.cluster().await.expect("cluster").version, v2);

    // Источники.
    let source = TrustedSource {
        name: format!("mfu-{tag}"),
        tenant: Some(tenant),
        matcher: SourceMatch::Networks(vec!["192.0.2.0/28".parse().expect("net")]),
        kind: SourceKind::Application {
            senders: vec![AddressPattern::new(&format!("scanner@{}", domain.name)).expect("p")],
            external: false,
        },
    };
    let v3 = store.put_source(v2, &source).await.expect("put source");
    assert!(v3 > v2);
    let snap = store.cluster().await.expect("cluster");
    assert!(snap.sources.contains(&source));
    let foreign = TrustedSource {
        tenant: Some(other),
        ..source.clone()
    };
    assert!(matches!(
        store.put_source(v3, &foreign).await,
        Err(Error::Invalid(_))
    ));
    let orphan = TrustedSource {
        tenant: None,
        name: format!("orphan-{tag}"),
        ..source.clone()
    };
    assert!(matches!(
        store.put_source(v3, &orphan).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        store.delete_source(v2, &source.name).await,
        Err(Error::Conflict)
    );

    // Политики доменов назначения: версии организаций независимы, снимок не видит чужих политик.
    assert_eq!(store.tenant(tenant).await, Ok(TenantTransport::default()));
    let all = DestinationPolicy {
        pattern: "*".into(),
        tls: TlsRequirement::Opportunistic,
        smart_host: Some("relay.example:587".into()),
        egress_ip: Some("198.51.100.10".parse().expect("ip")),
    };
    let partner = DestinationPolicy {
        pattern: "*.partner.example".into(),
        tls: TlsRequirement::Pinned {
            spki_sha256: vec!["0f".repeat(32)],
        },
        smart_host: None,
        egress_ip: None,
    };
    let t1 = store
        .put_route(tenant, 0, &partner)
        .await
        .expect("put route");
    let t2 = store.put_route(tenant, t1, &all).await.expect("put route");
    assert!(t2 > t1 && t1 > 0);
    let snap = store.tenant(tenant).await.expect("tenant");
    assert_eq!(snap.version, t2);
    assert_eq!(snap.routes, vec![all.clone(), partner.clone()]);
    assert_eq!(snap.route("mx.partner.example"), Some(&partner));
    assert_eq!(store.tenant(other).await, Ok(TenantTransport::default()));
    assert_eq!(
        store.put_route(tenant, t1, &all).await,
        Err(Error::Conflict)
    );
    assert!(matches!(
        store
            .put_route(
                tenant,
                t2,
                &DestinationPolicy {
                    pattern: "*.".into(),
                    ..all.clone()
                }
            )
            .await,
        Err(Error::Invalid(_))
    ));
    let o1 = store
        .put_route(other, 0, &partner)
        .await
        .expect("other route");
    assert_eq!(
        store.tenant(other).await.expect("other").routes,
        vec![partner.clone()]
    );
    assert_eq!(store.tenant(tenant).await.expect("tenant").version, t2);
    assert_eq!(
        store.delete_route(tenant, t2, "x.example").await,
        Err(Error::NotFound)
    );
    let t3 = store
        .delete_route(tenant, t2, &partner.pattern)
        .await
        .expect("delete route");
    assert_eq!(
        store.tenant(tenant).await.expect("tenant").routes,
        vec![all.clone()]
    );
    assert_eq!(
        store.tenant(other).await.expect("other").routes,
        vec![partner.clone()]
    );
    store
        .delete_route(tenant, t3, &all.pattern)
        .await
        .expect("delete route");
    store
        .delete_route(other, o1, &partner.pattern)
        .await
        .expect("delete other route");

    // Удаление: отсутствующий ключ — NotFound, устаревшая версия — Conflict.
    let v4 = store
        .delete_source(v3, &source.name)
        .await
        .expect("delete source");
    assert_eq!(
        store.delete_source(v4, &source.name).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.delete_domain(v3, domain.id).await,
        Err(Error::Conflict)
    );
    let v5 = store
        .delete_domain(v4, domain.id)
        .await
        .expect("delete domain");
    assert!(v5 > v4);
    assert_eq!(
        store.delete_domain(v5, domain.id).await,
        Err(Error::NotFound)
    );
    let snap = store.cluster().await.expect("cluster");
    assert_eq!(snap.domain(&domain.name), None);
    assert!(!snap.sources.iter().any(|s| s.name == source.name));
}

/// Набор проверок таблицы переписывания адресов `TransportStore` (CTR-40): правила — в снимке организации по
/// возрастанию `from`, общая версия с политиками доменов назначения, ключ — `from` (без учёта регистра),
/// проверка полей, изоляция организаций. Пишет только две новые организации.
pub async fn transport_rewrites(store: &dyn TransportStore) {
    let (tenant, other) = (TenantId::new(), TenantId::new());
    let pattern = |p: &str| AddressPattern::new(p).expect("pattern");
    let rule = |from: &str, to: &str| RewriteRule {
        from: pattern(from),
        to: pattern(to),
        sender: true,
        recipient: true,
    };
    let domain = rule("@old.example", "@example.org");
    let boss = RewriteRule {
        recipient: false,
        ..rule("boss@old.example", "ceo@example.org")
    };

    let t1 = store
        .put_rewrite(tenant, 0, &domain)
        .await
        .expect("put rewrite");
    assert!(t1 > 0);
    let route = DestinationPolicy {
        pattern: "*".into(),
        tls: TlsRequirement::Opportunistic,
        smart_host: None,
        egress_ip: None,
    };
    let t2 = store.put_route(tenant, t1, &route).await.expect("route");
    let t3 = store.put_rewrite(tenant, t2, &boss).await.expect("put");
    assert!(t3 > t2 && t2 > t1);
    let snap = store.tenant(tenant).await.expect("tenant");
    assert_eq!(snap.version, t3);
    assert_eq!(snap.routes, vec![route.clone()]);
    assert_eq!(snap.rewrites, vec![domain.clone(), boss.clone()]);
    assert_eq!(
        snap.rewrite("Boss@Old.Example", EnvelopeSide::Sender)
            .as_deref(),
        Some("ceo@example.org")
    );
    assert_eq!(
        snap.rewrite("boss@old.example", EnvelopeSide::Recipient)
            .as_deref(),
        Some("boss@example.org")
    );
    assert_eq!(store.tenant(other).await, Ok(TenantTransport::default()));

    // Устаревшая версия — Conflict, поля — Invalid; снимок не меняется.
    assert_eq!(
        store.put_rewrite(tenant, t2, &domain).await,
        Err(Error::Conflict)
    );
    let nowhere = RewriteRule {
        sender: false,
        recipient: false,
        ..domain.clone()
    };
    assert!(matches!(
        store.put_rewrite(tenant, t3, &nowhere).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        store
            .put_rewrite(tenant, t3, &rule("@old.example", "@OLD.example"))
            .await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(store.tenant(tenant).await.expect("tenant").version, t3);

    // Замена по ключу `from` (шаблон канонический: регистр не создаёт второго правила).
    let renamed = rule("@OLD.Example", "@new.example");
    let t4 = store
        .put_rewrite(tenant, t3, &renamed)
        .await
        .expect("replace");
    let snap = store.tenant(tenant).await.expect("tenant");
    assert_eq!(snap.rewrites, vec![renamed.clone(), boss.clone()]);
    assert_eq!(
        snap.rewrite("ann@old.example", EnvelopeSide::Recipient)
            .as_deref(),
        Some("ann@new.example")
    );

    // Чужая организация не видит правил; удаление — NotFound для отсутствующего ключа.
    let o1 = store.put_rewrite(other, 0, &boss).await.expect("other");
    assert_eq!(
        store.tenant(other).await.expect("other").rewrites,
        vec![boss.clone()]
    );
    assert_eq!(
        store
            .delete_rewrite(tenant, t4, &pattern("x@old.example"))
            .await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.delete_rewrite(tenant, t3, &boss.from).await,
        Err(Error::Conflict)
    );
    let t5 = store
        .delete_rewrite(tenant, t4, &boss.from)
        .await
        .expect("delete");
    assert_eq!(
        store.tenant(tenant).await.expect("tenant").rewrites,
        vec![renamed.clone()]
    );
    assert_eq!(
        store.tenant(other).await.expect("other").rewrites,
        vec![boss.clone()]
    );
    let t6 = store
        .delete_rewrite(tenant, t5, &renamed.from)
        .await
        .expect("delete");
    store
        .delete_route(tenant, t6, &route.pattern)
        .await
        .expect("delete route");
    store
        .delete_rewrite(other, o1, &boss.from)
        .await
        .expect("delete other");
}

// ---------- QueueStore ----------

/// Набор проверок `QueueStore::postpone` (CTR-40): перенос получателя без попытки — `attempts` и `last_error`
/// не меняются, срок — новый; устаревший захват — `Conflict`, чужой или завершённый получатель — `NotFound`.
/// Очередь общая: записи набора — со сроками в 1971 году (раньше записей других проверок), захваты набора
/// берут только свою запись по `id`, лиза — 30 с.
pub async fn queue_postpone(queue: &dyn QueueStore, tenant: TenantId) {
    let t0 = OffsetDateTime::UNIX_EPOCH + time::Duration::days(365);
    let sec = time::Duration::seconds;
    let lease = Duration::from_secs(30);
    let id = queue
        .enqueue(
            None,
            OutMessage {
                tenant,
                blob: BlobKey {
                    tenant,
                    hash: fresh_hash(),
                    generation: Gen::INITIAL,
                },
                size: 100,
                from: "a@example.org".into(),
                dsn: DsnParams::default(),
                rcpts: vec![OutRcpt::new("b@example.net"), OutRcpt::new("c@example.net")],
                next_attempt: t0,
            },
        )
        .await
        .expect("enqueue");
    let claim_own = async |now: OffsetDateTime| {
        queue
            .claim(now, 1000, lease)
            .await
            .expect("claim")
            .into_iter()
            .find(|c| c.claim.id == id)
    };

    let c1 = claim_own(t0).await.expect("claimed");
    assert_eq!(c1.rcpts.len(), 2);
    queue
        .reschedule(&c1.claim, "b@example.net", t0 + sec(60), "451 later")
        .await
        .expect("reschedule");
    queue
        .postpone(&c1.claim, "c@example.net", t0 + sec(60))
        .await
        .expect("postpone");
    assert_eq!(
        queue.postpone(&c1.claim, "x@example.net", t0).await,
        Err(Error::NotFound)
    );
    assert!(claim_own(t0 + sec(45)).await.is_none(), "not due yet");

    let c2 = claim_own(t0 + sec(60)).await.expect("due");
    let mut got: Vec<_> = c2
        .rcpts
        .iter()
        .map(|r| (r.rcpt.address.as_str(), r.attempts, r.last_error.as_deref()))
        .collect();
    got.sort_unstable();
    assert_eq!(
        got,
        [
            ("b@example.net", 1, Some("451 later")),
            ("c@example.net", 0, None)
        ]
    );
    assert_eq!(
        queue
            .postpone(&c1.claim, "b@example.net", t0 + sec(120))
            .await,
        Err(Error::Conflict)
    );

    // Перенос после неудачи сохраняет причину и число попыток; завершённый получатель — NotFound.
    queue
        .postpone(&c2.claim, "b@example.net", t0 + sec(120))
        .await
        .expect("postpone failed rcpt");
    let delivered = RcptStatus::Delivered {
        reply: "250 ok".into(),
    };
    assert_eq!(
        queue
            .complete(&c2.claim, "c@example.net", delivered.clone())
            .await,
        Ok(false)
    );
    assert!(claim_own(t0 + sec(100)).await.is_none(), "postponed");
    let c3 = claim_own(t0 + sec(120)).await.expect("due again");
    assert_eq!(c3.rcpts.len(), 1);
    assert_eq!(
        (c3.rcpts[0].attempts, c3.rcpts[0].last_error.as_deref()),
        (1, Some("451 later"))
    );
    assert_eq!(
        queue.postpone(&c3.claim, "c@example.net", t0).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        queue.complete(&c3.claim, "b@example.net", delivered).await,
        Ok(true)
    );
}

// ---------- Outbound ----------

/// Набор проверок [`Outbound`] (CTR-28): `submit` возвращает ровно один статус на каждого получателя `rcpts`
/// в порядке запроса (адреса как переданы); `enqueue` принимает то же письмо. `rcpts` — не пусто, адреса, которые
/// реализация примет (для `tp_smtp::Delivery` — известные локальные или внешние при подключённой очереди).
pub async fn outbound_submit(ob: &dyn Outbound, tenant: TenantId, rcpts: &[String]) {
    assert!(!rcpts.is_empty(), "набор: нужен хотя бы один получатель");
    let raw = Bytes::from_static(
        b"From: suite@example.org\r\nTo: suite@example.org\r\nSubject: outbound suite\r\n\r\nbody\r\n",
    );
    let sent = ob
        .submit(tenant, "suite@example.org", rcpts, raw.clone())
        .await
        .expect("submit");
    let got: Vec<&str> = sent.rcpts.iter().map(|(r, _)| r.as_str()).collect();
    let want: Vec<&str> = rcpts.iter().map(String::as_str).collect();
    assert_eq!(got, want, "статусы — по получателю в порядке запроса");
    assert!(
        sent.rcpts
            .iter()
            .all(|(_, s)| !matches!(s, SubmitStatus::Failed(_))),
        "принимаемые получатели не отклоняются: {:?}",
        sent.rcpts
    );
    ob.enqueue(tenant, "suite@example.org", rcpts, raw)
        .await
        .expect("enqueue");
}

// ---------- Соответствие Exchange (CTR-35) ----------

/// Набор проверок [`MailboxAudit`] (CTR-35): политика по умолчанию (не владелец — да, владелец — нет, чтение
/// владельцем — никогда), политика ящика и организации, поиск со страницами и условиями, изоляция организаций.
/// Моменты — текущие (журнал может быть секционирован по суткам); `purge` набор не проверяет. Политика организации
/// `tenant` в конце сбрасывается.
pub async fn mailbox_audit(audit: &dyn MailboxAudit, tenant: TenantId, foreign_tenant: TenantId) {
    use AuditActorKind as K;
    let owner = AccountScope {
        tenant,
        account: AccountId::new(),
    };
    let delegate = AccountId::new();
    let now = OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("time");
    let entry = |kind: AuditActorKind, action: AuditAction, s: i64| AuditEntry {
        scope: owner,
        at: now - time::Duration::seconds(100 - s),
        actor: match kind {
            K::Owner => "owner@example.org".into(),
            K::Delegate => "delegate@example.org".into(),
            K::Admin => "admin-subject".into(),
        },
        actor_account: match kind {
            K::Owner => Some(owner.account),
            K::Delegate => Some(delegate),
            K::Admin => None,
        },
        actor_kind: kind,
        action,
        object: AuditObject {
            detail: Some(format!("step {s}")),
            ..AuditObject::default()
        },
        client: Some("imap 192.0.2.1".into()),
    };
    assert_eq!(
        audit.audit_policy(owner).await.expect("policy"),
        AuditPolicy::default()
    );
    let rec = async |e: AuditEntry| audit.record_audit(e).await.expect("record");
    assert!(rec(entry(K::Delegate, AuditAction::Read, 1)).await);
    assert!(rec(entry(K::Admin, AuditAction::Login, 2)).await);
    assert!(!rec(entry(K::Owner, AuditAction::Delete, 3)).await);
    let mut foreign_owner = entry(K::Owner, AuditAction::Delete, 3);
    foreign_owner.actor_account = Some(delegate);
    assert!(matches!(
        audit.record_audit(foreign_owner).await,
        Err(Error::Invalid(_))
    ));

    // Политика ящика: удаление владельцем — да, перемещение — нет, чтение — никогда.
    let only_delete = AuditPolicy {
        non_owner: true,
        owner: [AuditAction::Delete].into_iter().collect(),
    };
    audit
        .set_audit_policy(tenant, Some(owner.account), Some(only_delete.clone()))
        .await
        .expect("mailbox policy");
    assert!(rec(entry(K::Owner, AuditAction::Delete, 4)).await);
    assert!(!rec(entry(K::Owner, AuditAction::Move, 5)).await);
    assert!(!rec(entry(K::Owner, AuditAction::Read, 6)).await);
    let reading = AuditPolicy {
        non_owner: true,
        owner: [AuditAction::Read].into_iter().collect(),
    };
    assert!(matches!(
        audit.set_audit_policy(tenant, None, Some(reading)).await,
        Err(Error::Invalid(_))
    ));

    // Политика организации — для ящиков без своей; сброс возвращает значение по умолчанию.
    let other = AccountScope {
        tenant,
        account: AccountId::new(),
    };
    audit
        .set_audit_policy(tenant, None, Some(AuditPolicy::all()))
        .await
        .expect("tenant policy");
    assert_eq!(
        audit.audit_policy(other).await.expect("other"),
        AuditPolicy::all()
    );
    assert_eq!(audit.audit_policy(owner).await.expect("owner"), only_delete);
    audit
        .set_audit_policy(tenant, None, None)
        .await
        .expect("reset tenant");
    assert_eq!(
        audit.audit_policy(other).await.expect("other reset"),
        AuditPolicy::default()
    );
    audit
        .set_audit_policy(tenant, Some(owner.account), None)
        .await
        .expect("reset mailbox");
    assert_eq!(
        audit.audit_policy(owner).await.expect("owner reset"),
        AuditPolicy::default()
    );

    // Поиск: записи ящика по времени, страницы, условия.
    let q = AuditQuery {
        account: Some(owner.account),
        ..AuditQuery::all(2)
    };
    let steps = |p: &AuditPage| -> Vec<(AuditActorKind, AuditAction)> {
        p.entries.iter().map(|e| (e.actor_kind, e.action)).collect()
    };
    let p1 = audit.search_audit(tenant, &q).await.expect("page 1");
    assert_eq!(
        steps(&p1),
        [
            (K::Delegate, AuditAction::Read),
            (K::Admin, AuditAction::Login)
        ]
    );
    assert_eq!(p1.entries[0], entry(K::Delegate, AuditAction::Read, 1));
    let p2 = audit
        .search_audit(
            tenant,
            &AuditQuery {
                after: p1.next.clone(),
                ..q.clone()
            },
        )
        .await
        .expect("page 2");
    assert_eq!(steps(&p2), [(K::Owner, AuditAction::Delete)]);
    assert_eq!(p2.next, None);
    let owner_only = audit
        .search_audit(
            tenant,
            &AuditQuery {
                account: Some(owner.account),
                actor_kind: Some(K::Owner),
                ..AuditQuery::all(10)
            },
        )
        .await
        .expect("by kind");
    assert_eq!(steps(&owner_only), [(K::Owner, AuditAction::Delete)]);
    let by_actor = audit
        .search_audit(
            tenant,
            &AuditQuery {
                account: Some(owner.account),
                actor: Some("DELEGATE@example.org".into()),
                ..AuditQuery::all(10)
            },
        )
        .await
        .expect("by actor");
    assert_eq!(steps(&by_actor), [(K::Delegate, AuditAction::Read)]);
    let alien = audit
        .search_audit(foreign_tenant, &q)
        .await
        .expect("foreign");
    assert!(alien.entries.is_empty());
    assert!(matches!(
        audit.search_audit(tenant, &AuditQuery::all(0)).await,
        Err(Error::Invalid(_))
    ));
    let garbage = AuditQuery {
        after: Some(AuditCursor("garbage".into())),
        ..q
    };
    assert!(matches!(
        audit.search_audit(tenant, &garbage).await,
        Err(Error::Invalid(_))
    ));
}

/// Набор проверок [`SenderLists`] (CTR-35): пустые списки нового ящика, замена с приведением записей, состояние,
/// вердикт (адрес приоритетнее домена, надёжный — заблокированного), некорректный список, изоляция ящиков.
pub async fn sender_lists(lists: &dyn SenderLists, scope: AccountScope, other: AccountScope) {
    assert_eq!(
        lists.sender_list(scope).await.expect("empty"),
        SenderList::default()
    );
    assert_eq!(lists.sender_list_state(scope).await.expect("state 0"), 0);
    let s1 = lists
        .put_sender_list(
            scope,
            SenderList {
                safe: vec!["Boss@Partner.example".into(), "@trusted.example".into()],
                blocked: vec!["partner.example".into(), "spam.example".into()],
            },
        )
        .await
        .expect("put");
    assert!(s1 > 0);
    assert_eq!(lists.sender_list_state(scope).await.expect("state"), s1);
    assert_eq!(
        lists.sender_list(scope).await.expect("get"),
        SenderList {
            safe: vec!["boss@partner.example".into(), "trusted.example".into()],
            blocked: vec!["partner.example".into(), "spam.example".into()],
        }
    );
    for (from, want) in [
        ("boss@partner.example", Some(SenderVerdict::Safe)),
        ("clerk@partner.example", Some(SenderVerdict::Blocked)),
        ("anyone@TRUSTED.example", Some(SenderVerdict::Safe)),
        ("x@sub.spam.example", None),
        ("", None),
    ] {
        assert_eq!(
            lists.sender_verdict(scope, from).await.expect("verdict"),
            want,
            "{from}"
        );
    }
    let bad = SenderList {
        safe: vec!["not an address".into()],
        blocked: vec![],
    };
    assert!(matches!(
        lists.put_sender_list(scope, bad).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(lists.sender_list_state(scope).await.expect("unchanged"), s1);
    assert_eq!(
        lists.sender_list(other).await.expect("other"),
        SenderList::default()
    );
    assert_eq!(
        lists
            .sender_verdict(other, "x@spam.example")
            .await
            .expect("other verdict"),
        None
    );
    let s2 = lists
        .put_sender_list(scope, SenderList::default())
        .await
        .expect("clear");
    assert!(s2 > s1);
    assert_eq!(
        lists
            .sender_verdict(scope, "x@spam.example")
            .await
            .expect("cleared"),
        None
    );
}

fn mail_contact(tenant: TenantId, email: String, external_id: Option<String>) -> MailContact {
    MailContact {
        id: ContactId::new(),
        tenant,
        display_name: "Партнёр".into(),
        email,
        external_id,
        attributes: Default::default(),
        groups: Vec::new(),
        hidden_from_gal: false,
    }
}

fn sorted<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort_unstable();
    v
}

/// Набор проверок [`ContactStore`] (CTR-35): создание и замена, поиск по адресу (без учёта регистра) и по
/// идентификатору каталога, занятый адрес или `external_id` — `Conflict`, состав групп, удаление, изоляция
/// организаций. Адреса — с уникальной меткой.
pub async fn contact_store(store: &dyn ContactStore, tenant: TenantId, foreign_tenant: TenantId) {
    let tag = AccountId::new().to_string();
    let addr = |who: &str| format!("{who}-{tag}@partner.example");
    let ext = format!("ext-{tag}");
    let (g1, g2) = (GroupId::new(), GroupId::new());
    let mut c1 = mail_contact(tenant, addr("ann"), Some(ext.clone()));
    c1.groups = vec![g2, g1, g2];
    store.put_contact(c1.clone()).await.expect("put c1");
    let got = store
        .contact(tenant, c1.id)
        .await
        .expect("get")
        .expect("c1");
    assert_eq!(got.groups, sorted(vec![g1, g2]));
    let found = store
        .contact_by_address(tenant, &addr("ann").to_uppercase())
        .await
        .expect("by address");
    assert_eq!(found.map(|c| c.id), Some(c1.id));
    let found = store
        .contact_by_external_id(tenant, &ext)
        .await
        .expect("by ext");
    assert_eq!(found.map(|c| c.id), Some(c1.id));
    let twin = mail_contact(tenant, addr("Ann"), None);
    assert_eq!(store.put_contact(twin).await, Err(Error::Conflict));
    let same_ext = mail_contact(tenant, addr("bob"), Some(ext.clone()));
    assert_eq!(store.put_contact(same_ext).await, Err(Error::Conflict));
    let bad = mail_contact(tenant, "no-at-sign".into(), None);
    assert!(matches!(
        store.put_contact(bad).await,
        Err(Error::Invalid(_))
    ));

    let mut c2 = mail_contact(tenant, addr("bob"), None);
    c2.groups = vec![g1];
    store.put_contact(c2.clone()).await.expect("put c2");
    let want = sorted(vec![c1.id, c2.id]);
    let in_g1: Vec<_> = store
        .group_contacts(tenant, g1)
        .await
        .expect("g1")
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(in_g1, want, "group contacts by id");
    let listed: Vec<_> = store
        .contacts(tenant)
        .await
        .expect("list")
        .into_iter()
        .map(|c| c.id)
        .filter(|id| want.contains(id))
        .collect();
    assert_eq!(listed, want, "list by id");

    // Замена: прежний адрес освобождается.
    let moved = MailContact {
        email: addr("ann-new"),
        ..c1.clone()
    };
    store.put_contact(moved).await.expect("replace");
    assert_eq!(
        store
            .contact_by_address(tenant, &addr("ann"))
            .await
            .expect("old"),
        None
    );
    assert_eq!(
        store.contact(foreign_tenant, c1.id).await.expect("foreign"),
        None
    );
    assert_eq!(
        store
            .contact_by_address(foreign_tenant, &addr("ann-new"))
            .await
            .expect("foreign address"),
        None
    );
    store.remove_contact(tenant, c1.id).await.expect("remove");
    assert_eq!(
        store.remove_contact(tenant, c1.id).await,
        Err(Error::NotFound)
    );
    store
        .remove_contact(tenant, c2.id)
        .await
        .expect("remove c2");
    assert!(
        store
            .group_contacts(tenant, g1)
            .await
            .expect("g1 empty")
            .is_empty()
    );
}

fn org_unit(tenant: TenantId, name: &str, parent: Option<OrgUnitId>) -> OrgUnit {
    OrgUnit {
        id: OrgUnitId::new(),
        tenant,
        name: name.into(),
        parent,
        external_id: None,
    }
}

/// Набор проверок [`OrgStore`] (CTR-35): дерево подразделений (неизвестный родитель и цикл — `Invalid`, занятый
/// `external_id` — `Conflict`, удаление с вложенными — `Conflict`), оргатрибуты, члены подразделения по
/// старшинству, сброс подразделения при удалении. `accounts` — не менее трёх существующих учётных записей
/// организации без оргатрибутов; в конце оргатрибуты сбрасываются.
pub async fn org_store(store: &dyn OrgStore, tenant: TenantId, accounts: &[AccountId]) {
    let [a1, a2, a3] = [accounts[0], accounts[1], accounts[2]];
    let tag = AccountId::new().to_string();
    let root = OrgUnit {
        external_id: Some(format!("ou-{tag}")),
        ..org_unit(tenant, "Компания", None)
    };
    let it = org_unit(tenant, "ИТ", Some(root.id));
    let dev = org_unit(tenant, "Разработка", Some(it.id));
    for u in [&root, &it, &dev] {
        store.put_org_unit(u.clone()).await.expect("put unit");
    }
    let orphan = org_unit(tenant, "Сирота", Some(OrgUnitId::new()));
    assert!(matches!(
        store.put_org_unit(orphan).await,
        Err(Error::Invalid(_))
    ));
    let looped = OrgUnit {
        parent: Some(dev.id),
        ..root.clone()
    };
    assert!(matches!(
        store.put_org_unit(looped).await,
        Err(Error::Invalid(_))
    ));
    let twin = OrgUnit {
        external_id: root.external_id.clone(),
        ..org_unit(tenant, "Двойник", None)
    };
    assert_eq!(store.put_org_unit(twin).await, Err(Error::Conflict));
    let renamed = OrgUnit {
        name: "Информационные технологии".into(),
        ..it.clone()
    };
    store.put_org_unit(renamed).await.expect("rename");
    let units = store.org_units(tenant).await.expect("units");
    let path: Vec<_> = unit_path(&units, dev.id)
        .iter()
        .map(|u| u.name.clone())
        .collect();
    assert_eq!(
        path,
        ["Компания", "Информационные технологии", "Разработка"]
    );

    let engineer = OrgAttributes {
        department: Some(dev.id),
        title: Some("Инженер".into()),
        manager: Some(a2),
        seniority: Some(10),
        attributes: [("office".to_owned(), format!("Офис {tag}"))]
            .into_iter()
            .collect(),
    };
    store
        .set_org(tenant, a1, engineer.clone())
        .await
        .expect("a1");
    let head = OrgAttributes {
        department: Some(it.id),
        seniority: Some(20),
        ..OrgAttributes::default()
    };
    store.set_org(tenant, a2, head).await.expect("a2");
    let junior = OrgAttributes {
        department: Some(dev.id),
        ..OrgAttributes::default()
    };
    store.set_org(tenant, a3, junior).await.expect("a3");
    assert_eq!(store.org(tenant, a1).await.expect("org a1"), engineer);
    assert_eq!(
        store.org(tenant, AccountId::new()).await.expect("none"),
        OrgAttributes::default()
    );
    let unknown_unit = OrgAttributes {
        department: Some(OrgUnitId::new()),
        ..OrgAttributes::default()
    };
    assert!(matches!(
        store.set_org(tenant, a1, unknown_unit).await,
        Err(Error::Invalid(_))
    ));
    let own_manager = OrgAttributes {
        manager: Some(a1),
        ..OrgAttributes::default()
    };
    assert!(matches!(
        store.set_org(tenant, a1, own_manager).await,
        Err(Error::Invalid(_))
    ));

    let ids = |m: Vec<OrgMember>| m.into_iter().map(|m| m.account).collect::<Vec<_>>();
    // a1 старше (10), a3 без старшинства; руководитель ИТ a2 — 20.
    let dev_members = store
        .org_members(tenant, Some(dev.id), false)
        .await
        .expect("dev");
    assert_eq!(ids(dev_members), [a1, a3]);
    let subtree = store
        .org_members(tenant, Some(it.id), true)
        .await
        .expect("it subtree");
    assert_eq!(ids(subtree), [a2, a1, a3]);
    let it_only = store
        .org_members(tenant, Some(it.id), false)
        .await
        .expect("it");
    assert_eq!(ids(it_only), [a2]);
    assert!(
        store
            .org_members(tenant, Some(root.id), false)
            .await
            .expect("root")
            .is_empty()
    );
    assert_eq!(
        store
            .org_members(tenant, Some(OrgUnitId::new()), true)
            .await,
        Err(Error::NotFound)
    );

    assert_eq!(
        store.remove_org_unit(tenant, it.id).await,
        Err(Error::Conflict)
    );
    store
        .remove_org_unit(tenant, dev.id)
        .await
        .expect("remove dev");
    assert_eq!(
        store.remove_org_unit(tenant, dev.id).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.org(tenant, a1).await.expect("a1 after").department,
        None
    );
    assert_eq!(
        store.org(tenant, a3).await.expect("a3 after"),
        OrgAttributes::default()
    );
    for id in [it.id, root.id] {
        store.remove_org_unit(tenant, id).await.expect("cleanup");
    }
    for a in [a1, a2] {
        store
            .set_org(tenant, a, OrgAttributes::default())
            .await
            .expect("reset");
    }
}

/// Окружение набора [`dynamic_groups`].
#[derive(Debug, Clone)]
pub struct DynamicFixture {
    /// Организация.
    pub tenant: TenantId,
    /// Существующая группа рассылки организации без статических членов.
    pub group: GroupId,
    /// Не менее трёх существующих учётных записей организации без оргатрибутов.
    pub accounts: Vec<AccountId>,
}

/// Набор проверок [`DynamicGroupStore`] (CTR-35): фильтр по атрибутам и подразделению, учётные записи и контакты
/// в составе, некорректный фильтр — `Invalid`, снятие фильтра. Значения атрибутов — с уникальной меткой.
pub async fn dynamic_groups(
    contacts: &dyn ContactStore,
    org: &dyn OrgStore,
    dynamic: &dyn DynamicGroupStore,
    fx: &DynamicFixture,
) {
    let t = fx.tenant;
    let [a1, a2, a3] = [fx.accounts[0], fx.accounts[1], fx.accounts[2]];
    let tag = AccountId::new().to_string();
    let office = format!("Москва {tag}");
    let unit = org_unit(t, "Отдел", None);
    org.put_org_unit(unit.clone()).await.expect("unit");
    let with_office = |department: Option<OrgUnitId>, title: &str| OrgAttributes {
        department,
        title: Some(title.into()),
        attributes: [("office".to_owned(), office.clone())]
            .into_iter()
            .collect(),
        ..OrgAttributes::default()
    };
    org.set_org(t, a1, with_office(Some(unit.id), "Инженер"))
        .await
        .expect("a1");
    org.set_org(t, a2, with_office(None, "Бухгалтер"))
        .await
        .expect("a2");
    let other_office = OrgAttributes {
        title: Some("Инженер".into()),
        ..OrgAttributes::default()
    };
    org.set_org(t, a3, other_office).await.expect("a3");
    let mut partner = mail_contact(t, format!("p-{tag}@partner.example"), None);
    partner.attributes = [("Office".to_owned(), office.to_uppercase())]
        .into_iter()
        .collect();
    contacts
        .put_contact(partner.clone())
        .await
        .expect("contact");

    let by_office = MemberFilter::Equals {
        attribute: "office".into(),
        value: office.clone(),
    };
    let mut g = DynamicGroup {
        group: fx.group,
        tenant: t,
        filter: by_office.clone(),
        contacts: false,
    };
    dynamic.put_dynamic(g.clone()).await.expect("put");
    assert_eq!(
        dynamic.dynamic(t, fx.group).await.expect("get"),
        Some(g.clone())
    );
    let listed = dynamic.dynamic_groups(t).await.expect("list");
    assert!(listed.iter().any(|x| x.group == fx.group));
    let both = sorted(vec![a1, a2]);
    assert_eq!(
        dynamic.expand_dynamic(t, fx.group).await.expect("expand"),
        Members {
            accounts: both.clone(),
            contacts: Vec::new(),
        }
    );
    g.contacts = true;
    dynamic.put_dynamic(g.clone()).await.expect("with contacts");
    assert_eq!(
        dynamic
            .expand_dynamic(t, fx.group)
            .await
            .expect("expand contacts"),
        Members {
            accounts: both,
            contacts: vec![partner.id],
        }
    );
    g.filter = MemberFilter::All(vec![
        by_office,
        MemberFilter::Unit {
            unit: unit.id,
            subtree: true,
        },
    ]);
    dynamic.put_dynamic(g.clone()).await.expect("by unit");
    assert_eq!(
        dynamic
            .expand_dynamic(t, fx.group)
            .await
            .expect("expand unit"),
        Members {
            accounts: vec![a1],
            contacts: Vec::new(),
        }
    );
    let bad = DynamicGroup {
        filter: MemberFilter::Present {
            attribute: "bad attribute".into(),
        },
        ..g
    };
    assert!(matches!(
        dynamic.put_dynamic(bad).await,
        Err(Error::Invalid(_))
    ));

    dynamic.remove_dynamic(t, fx.group).await.expect("remove");
    assert_eq!(
        dynamic.remove_dynamic(t, fx.group).await,
        Err(Error::NotFound)
    );
    assert_eq!(dynamic.dynamic(t, fx.group).await.expect("gone"), None);
    assert_eq!(
        dynamic.expand_dynamic(t, fx.group).await,
        Err(Error::NotFound)
    );

    contacts
        .remove_contact(t, partner.id)
        .await
        .expect("cleanup contact");
    for a in [a1, a2, a3] {
        org.set_org(t, a, OrgAttributes::default())
            .await
            .expect("reset");
    }
    org.remove_org_unit(t, unit.id).await.expect("cleanup unit");
}

/// Набор проверок [`SyncGate`] (CTR-35): остановка, подтверждение (сохраняется, если удаляемых не больше), снятие.
pub async fn sync_gate(gate: &dyn SyncGate, tenant: TenantId) {
    let at = OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("time");
    gate.clear_sync(tenant).await.expect("clear");
    assert_eq!(gate.pending_sync(tenant).await.expect("none"), None);
    assert_eq!(gate.approve_sync(tenant).await, Err(Error::NotFound));
    let h = gate.hold_sync(tenant, 10, 100, at).await.expect("hold");
    assert_eq!(
        h,
        MassRemoval {
            removed: 10,
            total: 100,
            detected_at: at,
            approved: false,
        }
    );
    assert!(gate.approve_sync(tenant).await.expect("approve").approved);
    assert!(
        gate.hold_sync(tenant, 8, 100, at)
            .await
            .expect("fewer")
            .approved
    );
    assert!(
        !gate
            .hold_sync(tenant, 12, 100, at)
            .await
            .expect("more")
            .approved
    );
    let pending = gate.pending_sync(tenant).await.expect("pending");
    assert_eq!(pending.map(|h| h.removed), Some(12));
    gate.clear_sync(tenant).await.expect("clear");
    gate.clear_sync(tenant).await.expect("clear again");
    assert_eq!(gate.pending_sync(tenant).await.expect("cleared"), None);
}

/// Окружение набора [`account_state`].
#[derive(Debug, Clone)]
pub struct AccountStateFixture {
    /// Организация.
    pub tenant: TenantId,
    /// Домен организации.
    pub domain: DomainId,
    /// Имя домена.
    pub domain_name: String,
}

/// Набор проверок состояния ящика `AccountStore` (CTR-35, CTR-50): отключение со сроком (`upsert` его не снимает),
/// списки отключённых и истёкших, восстановление, подключение к другой учётной записи (письма сохраняются, запись `to`
/// удаляется; письма ящика `to` — по режиму `ExistingContent`, без режима — `Invalid`), окончательное удаление
/// отключённого без писем.
pub async fn account_state(
    accounts: &dyn AccountStore,
    mailboxes: &dyn MailboxStore,
    messages: &dyn MessageStore,
    fx: &AccountStateFixture,
) {
    let t = fx.tenant;
    let run = AccountId::new();
    let upsert = |n: &str| AccountUpsert {
        domain: fx.domain,
        login: format!("{n}-{run}@{}", fx.domain_name),
        display_name: n.to_owned(),
        external_id: Some(format!("{n}-{run}")),
        enabled: true,
    };
    let scope = |account| AccountScope { tenant: t, account };
    let until = OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("time")
        + time::Duration::days(30);
    let sec = time::Duration::seconds;

    let a = accounts.upsert(t, upsert("leaver")).await.expect("a");
    assert_eq!(
        accounts.account_state(t, a.id).await.expect("state"),
        AccountState::Active
    );
    assert_eq!(
        accounts.account_state(t, AccountId::new()).await,
        Err(Error::NotFound)
    );
    accounts
        .disable_account(t, a.id, until)
        .await
        .expect("disable");
    let disabled = AccountState::Disabled { until };
    assert_eq!(
        accounts.account_state(t, a.id).await.expect("disabled"),
        disabled
    );
    accounts
        .upsert(t, upsert("leaver"))
        .await
        .expect("upsert again");
    assert_eq!(
        accounts
            .account_state(t, a.id)
            .await
            .expect("still disabled"),
        disabled
    );
    let listed = accounts.disabled_accounts(t).await.expect("disabled list");
    assert!(
        listed
            .iter()
            .any(|d| d.account.id == a.id && d.until == until)
    );
    let own = |v: Vec<AccountScope>| v.contains(&scope(a.id));
    assert!(!own(accounts
        .expired_accounts(until - sec(1), 10_000)
        .await
        .expect("not yet")));
    assert!(own(accounts
        .expired_accounts(until, 10_000)
        .await
        .expect("expired")));
    accounts.restore_account(t, a.id).await.expect("restore");
    accounts
        .restore_account(t, a.id)
        .await
        .expect("restore again");
    assert_eq!(
        accounts.account_state(t, a.id).await.expect("active"),
        AccountState::Active
    );
    let listed = accounts.disabled_accounts(t).await.expect("list");
    assert!(!listed.iter().any(|d| d.account.id == a.id));
    assert_eq!(
        accounts.disable_account(t, AccountId::new(), until).await,
        Err(Error::NotFound)
    );

    // Подключение: письма отключённого ящика — у учётной записи каталога `to`.
    let inbox = role_folder(mailboxes, scope(a.id), SpecialUse::Inbox).await;
    let kept = put_message(messages, scope(a.id), inbox, 10).await;
    let to = accounts.upsert(t, upsert("successor")).await.expect("to");
    assert!(matches!(
        accounts.attach_account(t, a.id, to.id, None).await,
        Err(Error::Invalid(_))
    ));
    accounts
        .disable_account(t, a.id, until)
        .await
        .expect("disable a");
    let attached = accounts
        .attach_account(t, a.id, to.id, None)
        .await
        .expect("attach");
    assert_eq!(
        (
            attached.id,
            attached.login.as_str(),
            attached.external_id.as_deref()
        ),
        (a.id, to.login.as_str(), to.external_id.as_deref())
    );
    assert_eq!(accounts.get(t, to.id).await.expect("to gone"), None);
    assert_eq!(
        accounts.account_state(t, a.id).await.expect("attached"),
        AccountState::Active
    );
    let by_login = accounts.by_login(t, &to.login).await.expect("by login");
    assert_eq!(by_login.map(|x| x.id), Some(a.id));
    assert!(
        messages
            .get(scope(a.id), kept)
            .await
            .expect("message")
            .is_some()
    );

    // В ящике `to` есть письма: без режима — Invalid (подсказка); `to` отключена — Conflict; нет записи — NotFound.
    let b = accounts.upsert(t, upsert("other")).await.expect("b");
    let busy = accounts.upsert(t, upsert("busy")).await.expect("busy");
    let busy_inbox = role_folder(mailboxes, scope(busy.id), SpecialUse::Inbox).await;
    let busy_msg = put_message(messages, scope(busy.id), busy_inbox, 10).await;
    accounts
        .disable_account(t, b.id, until)
        .await
        .expect("disable b");
    assert!(matches!(
        accounts.attach_account(t, b.id, busy.id, None).await,
        Err(Error::Invalid(_))
    ));
    let gone = accounts.upsert(t, upsert("gone")).await.expect("gone");
    accounts
        .disable_account(t, gone.id, until)
        .await
        .expect("disable gone");
    assert_eq!(
        accounts
            .attach_account(t, b.id, gone.id, Some(ExistingContent::Keep))
            .await,
        Err(Error::Conflict)
    );
    assert_eq!(
        accounts
            .attach_account(t, b.id, AccountId::new(), Some(ExistingContent::Keep))
            .await,
        Err(Error::NotFound)
    );

    // «Оставить»: письма системной папки — в папку с той же ролью, папка с занятым именем — с суффиксом.
    let folder = |parent| NewMailbox {
        parent,
        name: "Projects".to_owned(),
        role: None,
    };
    let own = mailboxes
        .create(scope(b.id), folder(None))
        .await
        .expect("own folder");
    let theirs = mailboxes
        .create(scope(busy.id), folder(None))
        .await
        .expect("their folder");
    let nested = mailboxes
        .create(scope(busy.id), folder(Some(theirs.id)))
        .await
        .expect("nested folder");
    let project_msg = put_message(messages, scope(busy.id), nested.id, 20).await;
    let attached = accounts
        .attach_account(t, b.id, busy.id, Some(ExistingContent::Keep))
        .await
        .expect("attach keep");
    assert_eq!(
        (attached.id, attached.login.as_str()),
        (b.id, busy.login.as_str())
    );
    assert_eq!(accounts.get(t, busy.id).await.expect("busy gone"), None);
    let b_inbox = role_folder(mailboxes, scope(b.id), SpecialUse::Inbox).await;
    let moved = messages
        .get(scope(b.id), busy_msg)
        .await
        .expect("get")
        .expect("inbox message kept");
    assert_eq!(moved.mailbox, b_inbox);
    let listed = mailboxes.list(scope(b.id)).await.expect("list b");
    let renamed = listed
        .iter()
        .find(|m| m.parent.is_none() && m.name == "Projects 1")
        .expect("moved folder with suffix");
    assert!(
        listed
            .iter()
            .any(|m| m.id == own.id && m.name == "Projects")
    );
    let moved_nested = listed
        .iter()
        .find(|m| m.parent == Some(renamed.id) && m.name == "Projects")
        .expect("nested folder moved");
    let moved = messages
        .get(scope(b.id), project_msg)
        .await
        .expect("get")
        .expect("folder message kept");
    assert_eq!(moved.mailbox, moved_nested.id);
    assert_eq!(
        mailboxes
            .status(scope(b.id), moved_nested.id)
            .await
            .expect("status")
            .messages,
        1
    );
    assert_eq!(accounts.quota(t, b.id).await.expect("quota").messages, 2);

    // «Удалить»: письма ящика `to` — в корзину подключаемого ящика, папки не переносятся.
    let c = accounts.upsert(t, upsert("third")).await.expect("c");
    let d = accounts.upsert(t, upsert("fourth")).await.expect("d");
    let d_inbox = role_folder(mailboxes, scope(d.id), SpecialUse::Inbox).await;
    let d_msg = put_message(messages, scope(d.id), d_inbox, 10).await;
    mailboxes
        .create(scope(d.id), folder(None))
        .await
        .expect("d folder");
    accounts
        .disable_account(t, c.id, until)
        .await
        .expect("disable c");
    accounts
        .attach_account(t, c.id, d.id, Some(ExistingContent::Delete))
        .await
        .expect("attach delete");
    let c_trash = role_folder(mailboxes, scope(c.id), SpecialUse::Trash).await;
    let trashed = messages
        .get(scope(c.id), d_msg)
        .await
        .expect("get")
        .expect("message in trash");
    assert_eq!(trashed.mailbox, c_trash);
    assert!(
        !mailboxes
            .list(scope(c.id))
            .await
            .expect("list c")
            .iter()
            .any(|m| m.name == "Projects")
    );

    // Окончательное удаление: только отключённый и без писем.
    assert!(matches!(
        accounts.remove_disabled(t, a.id).await,
        Err(Error::Invalid(_))
    ));
    accounts
        .disable_account(t, a.id, until)
        .await
        .expect("disable a again");
    assert_eq!(
        accounts.remove_disabled(t, a.id).await,
        Err(Error::Conflict)
    );
    accounts
        .remove_disabled(t, gone.id)
        .await
        .expect("remove gone");
    assert_eq!(accounts.get(t, gone.id).await.expect("gone removed"), None);
    assert_eq!(
        accounts.remove_disabled(t, gone.id).await,
        Err(Error::NotFound)
    );
}

/// Набор проверок срока выпуска и отмены `QueueStore` (CTR-35): запись не выдаётся до срока выпуска, до первого
/// захвата отменяется (возвращается объект письма), после захвата — `Conflict`; `queued_at` не раньше срока выпуска.
pub async fn queue_release(queue: &dyn QueueStore, tenant: TenantId) {
    let t0 = OffsetDateTime::UNIX_EPOCH + time::Duration::days(500);
    let sec = time::Duration::seconds;
    let lease = Duration::from_secs(30);
    let message = |release: OffsetDateTime| OutMessage {
        tenant,
        blob: BlobKey {
            tenant,
            hash: fresh_hash(),
            generation: Gen::INITIAL,
        },
        size: 100,
        from: "a@example.org".into(),
        dsn: DsnParams::default(),
        rcpts: vec![OutRcpt::new("b@example.net")],
        next_attempt: release,
    };
    let claim_own = async |id: QueueId, now: OffsetDateTime| {
        queue
            .claim(now, 1000, lease)
            .await
            .expect("claim")
            .into_iter()
            .find(|c| c.claim.id == id)
    };

    let delayed = message(t0 + sec(3600));
    let blob = delayed.blob;
    assert_eq!(delayed.release_at(), t0 + sec(3600));
    let id = queue.enqueue(None, delayed).await.expect("enqueue delayed");
    assert!(claim_own(id, t0).await.is_none(), "not released yet");
    assert_eq!(
        queue.cancel_queued(TenantId::new(), id).await,
        Err(Error::NotFound)
    );
    assert_eq!(queue.cancel_queued(tenant, id).await, Ok(blob));
    assert_eq!(queue.cancel_queued(tenant, id).await, Err(Error::NotFound));
    assert!(claim_own(id, t0 + sec(7200)).await.is_none(), "canceled");

    let id = queue
        .enqueue(None, message(t0 + sec(60)))
        .await
        .expect("enqueue");
    let c = claim_own(id, t0 + sec(60)).await.expect("released");
    assert!(c.queued_at >= t0 + sec(60), "queued_at from release");
    assert_eq!(queue.cancel_queued(tenant, id).await, Err(Error::Conflict));
    let done = RcptStatus::Delivered {
        reply: "250 ok".into(),
    };
    assert_eq!(
        queue.complete(&c.claim, "b@example.net", done).await,
        Ok(true)
    );
    assert_eq!(queue.cancel_queued(tenant, id).await, Err(Error::NotFound));
}

/// Набор проверок `DeliveryLog::by_queue` (CTR-35): события записи очереди по времени, ограничение числа, изоляция
/// организаций и записей. Моменты — текущие.
pub async fn delivery_by_queue(log: &dyn DeliveryLog, fx: &DeliveryFixture) {
    use DeliveryEventKind as K;
    let q = QueueId::new();
    let now = OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("time");
    let node = NodeId::new("suite-node").expect("node");
    let event = |tenant: TenantId, s: i64, kind: DeliveryEventKind, queue: QueueId| DeliveryEvent {
        tenant,
        at: now - time::Duration::seconds(100 - s),
        node: node.clone(),
        kind,
        sender: "a@example.org".into(),
        recipients: vec!["b@example.net".into()],
        message_id: Some(format!("{q}@example.org")),
        queue_id: Some(queue),
        decision: DeliveryDecision::Accept,
        reason: Some(format!("step {s}")),
    };
    let relayed = event(fx.tenant, 2, K::Relay, q);
    let received = event(fx.tenant, 1, K::Receive, q);
    log.record_batch(vec![
        relayed.clone(),
        received.clone(),
        event(fx.tenant, 3, K::Relay, QueueId::new()),
        event(fx.foreign_tenant, 4, K::Dsn, q),
    ])
    .await
    .expect("record");
    assert_eq!(
        log.by_queue(fx.tenant, q, 10).await.expect("by queue"),
        [received.clone(), relayed]
    );
    assert_eq!(
        log.by_queue(fx.tenant, q, 1).await.expect("limit"),
        [received]
    );
    assert!(matches!(
        log.by_queue(fx.tenant, q, 0).await,
        Err(Error::Invalid(_))
    ));
    let foreign = log
        .by_queue(fx.foreign_tenant, q, 10)
        .await
        .expect("foreign");
    let kinds: Vec<_> = foreign.iter().map(|e| e.kind).collect();
    assert_eq!(kinds, [K::Dsn]);
}

// ---------- Отложенная отправка JMAP (CTR-42) ----------

/// Набор проверок выборки, изменений и срока хранения [`SubmissionStore`] (CTR-42): `query_submissions` по условиям
/// RFC 8621 §7.3, `submission_changes` (созданные, изменённые, удалённые; созданные и удалённые после `since` не
/// входят; `limit` и `has_more`; неизвестное состояние — `Conflict`), `expire_submissions` (только завершённые и
/// отменённые старше срока). Моменты отправок — в 1971 году: срок хранения набора не задевает чужие записи.
pub async fn submission_query(
    store: &dyn SubmissionStore,
    owner: AccountScope,
    peer: AccountScope,
) {
    let t0 = OffsetDateTime::UNIX_EPOCH + time::Duration::days(400);
    let day = time::Duration::days;
    let sub =
        |id: &str, identity: &str, send_at: OffsetDateTime, undo: UndoStatus| EmailSubmission {
            id: id.into(),
            identity: identity.into(),
            email: MessageId::new(),
            mail_from: "anna@example.org".into(),
            rcpt_to: vec!["b@example.com".into()],
            send_at,
            undo_status: undo,
            delivery: Vec::new(),
            queue: Some(QueueId::new()),
        };
    let s0 = store.state(owner).await.expect("state");
    let a = sub("a", "work", t0 + day(1), UndoStatus::Pending);
    let b = sub("b", "work", t0, UndoStatus::Final);
    let c = sub("c", "home", t0 + day(2), UndoStatus::Pending);
    for s in [&a, &b, &c] {
        store.insert(owner, s.clone()).await.expect("insert");
    }
    let ids = |v: Vec<EmailSubmission>| v.into_iter().map(|s| s.id).collect::<Vec<_>>();
    let query =
        async |f: SubmissionFilter| ids(store.query_submissions(owner, &f).await.expect("query"));
    assert_eq!(query(SubmissionFilter::default()).await, ["b", "a", "c"]);
    let pending = SubmissionFilter {
        undo_status: Some(UndoStatus::Pending),
        ..SubmissionFilter::default()
    };
    assert_eq!(query(pending).await, ["a", "c"]);
    let home = SubmissionFilter {
        identity_ids: Some(vec!["home".into()]),
        ..SubmissionFilter::default()
    };
    assert_eq!(query(home).await, ["c"]);
    let by_email = SubmissionFilter {
        email_ids: Some(vec![b.email, c.email]),
        ..SubmissionFilter::default()
    };
    assert_eq!(query(by_email).await, ["b", "c"]);
    let window = SubmissionFilter {
        after: Some(t0 + day(1)),
        before: Some(t0 + day(2)),
        ..SubmissionFilter::default()
    };
    assert_eq!(query(window).await, ["a"]);
    let alien = store
        .query_submissions(peer, &SubmissionFilter::default())
        .await
        .expect("peer");
    assert!(
        alien
            .iter()
            .all(|s| !["a", "b", "c"].contains(&s.id.as_str()))
    );

    // Изменения: созданные после s0.
    let ch = store
        .submission_changes(owner, s0, 100)
        .await
        .expect("changes");
    assert_eq!(
        (ch.created.as_slice(), ch.updated.len(), ch.destroyed.len()),
        (["a", "b", "c"].map(String::from).as_slice(), 0, 0)
    );
    let s1 = store.state(owner).await.expect("s1");
    assert_eq!((ch.new_state, ch.has_more), (s1, false));

    // Отмена, фиксация, удаление; созданная и удалённая после s1 не входит.
    store.cancel(owner, "a").await.expect("cancel");
    store
        .finalize(owner, "c", vec![], c.queue)
        .await
        .expect("finalize");
    store.delete(owner, "b").await.expect("delete");
    store
        .insert(owner, sub("tmp", "work", t0, UndoStatus::Final))
        .await
        .expect("tmp");
    store.delete(owner, "tmp").await.expect("delete tmp");
    let ch = store
        .submission_changes(owner, s1, 100)
        .await
        .expect("changes s1");
    assert!(ch.created.is_empty());
    assert_eq!(ch.updated, ["a", "c"]);
    assert_eq!(ch.destroyed, ["b"]);
    let s2 = store.state(owner).await.expect("s2");
    assert_eq!(ch.new_state, s2);

    // По одной: `has_more` до последней порции, итог — тот же.
    let (mut since, mut all) = (s1, Vec::new());
    loop {
        let part = store
            .submission_changes(owner, since, 1)
            .await
            .expect("part");
        let n = part.created.len() + part.updated.len() + part.destroyed.len();
        assert!(n <= 1, "limit 1: {part:?}");
        all.extend(part.updated);
        all.extend(part.destroyed);
        since = part.new_state;
        if !part.has_more {
            break;
        }
    }
    all.sort_unstable();
    assert_eq!(all, ["a", "b", "c"]);
    assert_eq!(since, s2);
    assert_eq!(
        store.submission_changes(owner, s2 + 1, 10).await,
        Err(Error::Conflict)
    );
    assert!(matches!(
        store.submission_changes(owner, s1, 0).await,
        Err(Error::Invalid(_))
    ));

    // Срок хранения: отменённая `a` и завершённая `c` старше срока удаляются, ждущая и новая — нет.
    let waiting = sub("w", "work", t0, UndoStatus::Pending);
    store.insert(owner, waiting).await.expect("waiting");
    let fresh = sub("fresh", "work", t0 + day(30), UndoStatus::Final);
    store.insert(peer, fresh).await.expect("fresh");
    let s3 = store.state(owner).await.expect("s3");
    let removed = store
        .expire_submissions(t0 + day(10), 10_000)
        .await
        .expect("expire");
    assert!(removed >= 2, "removed {removed}");
    assert_eq!(store.get(owner, "a").await.expect("a"), None);
    assert_eq!(store.get(owner, "c").await.expect("c"), None);
    assert!(store.get(owner, "w").await.expect("w").is_some());
    assert!(store.get(peer, "fresh").await.expect("fresh").is_some());
    let ch = store
        .submission_changes(owner, s3, 100)
        .await
        .expect("after expire");
    assert_eq!(ch.destroyed, ["a", "c"]);
    store.cancel(owner, "w").await.expect("cleanup w");
}

/// Набор проверок отложенной отправки [`Outbound`] (CTR-42): `schedule` возвращает запись очереди, `cancel_scheduled`
/// до выпуска удаляет её (повтор и чужая организация — `NotFound`). `release_at` — в будущем относительно часов
/// реализации.
pub async fn outbound_schedule(
    ob: &dyn Outbound,
    tenant: TenantId,
    rcpts: &[String],
    release_at: OffsetDateTime,
) {
    let raw = Bytes::from_static(b"Subject: later\r\n\r\nbody\r\n");
    let id = ob
        .schedule(tenant, "anna@example.org", rcpts, raw.clone(), release_at)
        .await
        .expect("schedule");
    assert_eq!(
        ob.cancel_scheduled(TenantId::new(), id).await,
        Err(Error::NotFound)
    );
    ob.cancel_scheduled(tenant, id).await.expect("cancel");
    assert_eq!(ob.cancel_scheduled(tenant, id).await, Err(Error::NotFound));
    let other = ob
        .schedule(tenant, "anna@example.org", rcpts, raw, release_at)
        .await
        .expect("schedule again");
    assert_ne!(other, id);
    ob.cancel_scheduled(tenant, other)
        .await
        .expect("cancel other");
}

// ---------- Модерация правилом потока (CTR-43) ----------

/// Окружение набора [`rule_moderation`].
#[derive(Debug, Clone)]
pub struct RuleModerationFixture {
    /// Организация.
    pub tenant: TenantId,
    /// Модератор правила (учётная запись организации).
    pub moderator: AccountId,
    /// Исходные получатели писем (адреса, которые реализация `Moderation` доставит).
    pub recipients: Vec<String>,
}

/// Общий набор очереди модерации для правил потока (CTR-43): письмо без группы ставится в очередь (`GroupStore::hold`,
/// источник и модераторы сохраняются), видно в очереди организации, одобрение — `Moderation::deliver` без группы
/// (доставка исходным получателям), отказ — `Moderation::reject` (DSN отправителю); письмо правила с группой и без
/// модераторов — `Invalid`. Возвращает (одобренное, отклонённое) — что доставлено и какое уведомление ушло, проверяет
/// реализация.
pub async fn rule_moderation(
    groups: &dyn GroupStore,
    m: &dyn Moderation,
    fx: &RuleModerationFixture,
) -> (HeldId, HeldId) {
    let t = fx.tenant;
    let tag = AccountId::new().to_string();
    let at = OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("time");
    let held = |s: i64| HeldMessage {
        id: HeldId::new(),
        group: None,
        source: HoldSource::FlowRule {
            rule: format!("Правило {tag}"),
        },
        moderators: vec![Moderator::Account(fx.moderator)],
        recipients: fx.recipients.clone(),
        sender: format!("s-{tag}@example.net"),
        sender_account: None,
        message_id: Some(format!("{s}.{tag}@example.net")),
        blob: BlobKey {
            tenant: t,
            hash: fresh_hash(),
            generation: Gen::INITIAL,
        },
        size: 100,
        held_at: at + time::Duration::seconds(s),
    };
    let (approve, decline) = (held(1), held(2));
    for h in [&approve, &decline] {
        groups.hold(t, h.clone()).await.expect("hold by rule");
    }
    let unmoderated = HeldMessage {
        moderators: Vec::new(),
        ..held(3)
    };
    assert!(matches!(
        groups.hold(t, unmoderated).await,
        Err(Error::Invalid(_))
    ));
    let grouped = HeldMessage {
        group: Some(GroupId::new()),
        ..held(4)
    };
    assert!(matches!(
        groups.hold(t, grouped).await,
        Err(Error::Invalid(_))
    ));
    let queue: Vec<_> = groups
        .held(t, None, 10_000)
        .await
        .expect("queue")
        .into_iter()
        .filter(|h| h.message_id.as_deref().is_some_and(|m| m.contains(&tag)))
        .collect();
    assert_eq!(queue, [approve.clone(), decline.clone()]);
    let in_group = groups
        .held(t, Some(GroupId::new()), 10_000)
        .await
        .expect("group queue");
    assert!(in_group.iter().all(|h| h.id != approve.id));

    let taken = groups.take_held(t, approve.id).await.expect("take");
    assert_eq!(taken, approve);
    let some_group = DistributionGroup {
        id: GroupId::new(),
        tenant: t,
        name: "Другая".into(),
        address: None,
        external_id: None,
        members: Vec::new(),
        senders: SenderRestriction::Anyone,
        moderators: Vec::new(),
        hidden_from_gal: false,
    };
    assert!(matches!(
        m.deliver(t, Some(&some_group), &taken).await,
        Err(Error::Invalid(_))
    ));
    m.deliver(t, None, &taken).await.expect("deliver by rule");
    let taken = groups.take_held(t, decline.id).await.expect("take decline");
    m.reject(t, &taken, "not allowed").await.expect("reject");
    assert_eq!(groups.take_held(t, approve.id).await, Err(Error::NotFound));
    (approve.id, decline.id)
}
