//! Сигналы через LISTEN/NOTIFY (META-26): доставка и фильтрация, склейка пачек под нагрузкой, переполнение
//! буфера и переподключение слушателя — `Resync`; сигналы хранилищ после фиксации; контракт `EventBus` — общий
//! набор `tp_testkit::suite::signals` (META-36).

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use sqlx::PgPool;
use tp_contracts::abuse::BlockSubject;
use tp_contracts::bus::{Event, EventBus, EventKind};
use tp_contracts::events::{Partition, partition_of};
use tp_contracts::membership::{Membership, NodeId, NodeInfo};
use tp_contracts::meta::{AccountScope, MailboxStore, NewMailbox};
use tp_contracts::{AccountId, Error, TenantId};
use tp_meta::failover::{FailoverConfig, connect_failover};
use tp_meta::signals::{PgSignalBus, SignalsConfig, Subscription, connect_listener};
use tp_meta::{MetaConfig, PgMeta};
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(10);

/// Шина над тестовой БД с отдельным каналом (тесты не видят сигналов друг друга).
async fn bus(cfg: SignalsConfig) -> Option<(PgSignalBus, String, PgPool)> {
    let url = common::pg_url()?;
    let mut pool_cfg = common::dev_pool_config(&url);
    let app = format!("tp_sig_{}", Uuid::now_v7().simple());
    pool_cfg.application_name = app.clone();
    pool_cfg.failover = FailoverConfig {
        retry_initial: Duration::from_millis(20),
        retry_max: Duration::from_millis(200),
        retry_for: Duration::from_secs(10),
        probe_timeout: Duration::from_secs(2),
    };
    let publish = connect_failover(&pool_cfg, common::secrets())
        .await
        .expect("connect_failover");
    let admin = publish.pool().clone();
    let listen = connect_listener(&pool_cfg, common::secrets())
        .await
        .expect("connect_listener");
    let cfg = SignalsConfig {
        channel: format!("tp_t{}", Uuid::now_v7().simple()),
        ..cfg
    };
    let bus = PgSignalBus::start(publish, listen, cfg).expect("start");
    Some((bus, format!("{app} listener"), admin))
}

async fn next(sub: &mut Subscription) -> Event {
    tokio::time::timeout(WAIT, sub.next())
        .await
        .expect("сигнал не пришёл")
        .expect("подписка завершена")
}

fn system(tenant: TenantId, kind: EventKind) -> Event {
    Event {
        tenant,
        account: None,
        kind,
    }
}

fn log(p: u32) -> Event {
    system(
        TenantId::DEFAULT,
        EventKind::LogAppended {
            partition: Partition(p),
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_signals() {
    let Some((bus, _, _)) = bus(SignalsConfig::default()).await else {
        return;
    };
    let scope = AccountScope {
        tenant: TenantId::new(),
        account: AccountId::new(),
    };
    let mut sys = bus.subscribe_system().await.expect("subscribe_system");
    let mut acc = bus.subscribe(scope).await.expect("subscribe");
    let other = bus
        .subscribe(AccountScope {
            tenant: scope.tenant,
            account: AccountId::new(),
        })
        .await
        .expect("subscribe other");
    assert_eq!(bus.subscribers(), 3);

    let published = [
        log(7),
        system(TenantId::DEFAULT, EventKind::MembersChanged),
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
        system(scope.tenant, EventKind::TransportChanged { tenant: None }),
    ];
    for e in &published {
        bus.publish(e.clone()).await.expect("publish");
    }
    let mine = Event {
        tenant: scope.tenant,
        account: Some(scope.account),
        kind: EventKind::MailboxListChanged,
    };
    bus.publish(mine.clone()).await.expect("publish");
    assert_eq!(
        next(&mut acc).await,
        mine,
        "системные — не подписке учётной записи"
    );
    for e in published {
        assert_eq!(next(&mut sys).await, e);
    }
    // Подписка другой учётной записи сигналов этой не получает: первым приходит её собственный.
    let theirs = Event {
        tenant: scope.tenant,
        account: Some(AccountId::new()),
        kind: EventKind::MailboxListChanged,
    };
    drop(other);
    let mut other = bus
        .subscribe(AccountScope {
            tenant: scope.tenant,
            account: theirs.account.expect("account"),
        })
        .await
        .expect("subscribe");
    bus.publish(mine.clone()).await.expect("publish");
    bus.publish(theirs.clone()).await.expect("publish");
    assert_eq!(next(&mut other).await, theirs);
    assert_eq!(next(&mut acc).await, mine);

    for bad in [
        system(TenantId::DEFAULT, EventKind::Resync),
        Event {
            account: Some(scope.account),
            ..system(scope.tenant, EventKind::Resync)
        },
        Event {
            account: Some(scope.account),
            ..system(scope.tenant, EventKind::QueueReady)
        },
    ] {
        assert!(matches!(bus.publish(bad).await, Err(Error::Invalid(_))));
    }
    let m = bus.metrics();
    assert!(m.batches >= 1 && m.received >= 8, "{m:?}");
    assert!(m.latency_count >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batches_coalesce_under_load() {
    let cfg = SignalsConfig {
        subscriber_buffer: 1 << 20,
        ..SignalsConfig::default()
    };
    let Some((bus, _, _)) = bus(cfg).await else {
        return;
    };
    let mut sys = bus.subscribe_system().await.expect("subscribe_system");
    // 8 публикаторов × 2000 сигналов по 1000 разделам: повторы склеиваются, пачки — больше одного сигнала.
    let mut tasks = Vec::new();
    for w in 0..8u32 {
        let bus = bus.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..2000u32 {
                bus.publish(log((w * 7919 + i) % 1000))
                    .await
                    .expect("publish");
                if i % 64 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }
    for t in tasks {
        t.await.expect("task");
    }
    let end = system(TenantId::DEFAULT, EventKind::ConfigReloaded);
    bus.publish(end.clone()).await.expect("publish");
    let mut seen = HashSet::new();
    loop {
        let e = next(&mut sys).await;
        assert_ne!(e.kind, EventKind::Resync, "буфер не переполнялся");
        if e == end {
            break;
        }
        if let EventKind::LogAppended { partition } = e.kind {
            seen.insert(partition.0);
        }
    }
    assert_eq!(seen.len(), 1000, "каждый раздел — хотя бы один сигнал");
    let m = bus.metrics();
    assert_eq!(m.published, 16001);
    assert!(m.batched < m.published, "склейка: {m:?}");
    assert!(m.batches < m.batched, "пачки больше одного сигнала: {m:?}");
    assert!(m.batch_max > 1, "{m:?}");
    assert_eq!(m.dropped, 0);
    assert!(m.notify_total > Duration::ZERO);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overflow_gives_resync() {
    let cfg = SignalsConfig {
        subscriber_buffer: 4,
        ..SignalsConfig::default()
    };
    let Some((bus, _, _)) = bus(cfg).await else {
        return;
    };
    let mut slow = bus.subscribe_system().await.expect("subscribe_system");
    for p in 0..50 {
        bus.publish(log(p)).await.expect("publish");
    }
    let end = system(TenantId::DEFAULT, EventKind::ConfigReloaded);
    bus.publish(end.clone()).await.expect("publish");
    // Дождаться, пока слушатель примет всё (метрика), затем читать.
    tokio::time::timeout(WAIT, async {
        while bus.metrics().received < 51 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("слушатель принял сигналы");
    let mut got = Vec::new();
    loop {
        let e = next(&mut slow).await;
        let done = e == end;
        got.push(e);
        if done {
            break;
        }
    }
    assert!(got.len() <= 4, "буфер ограничен: {got:?}");
    assert!(
        got.contains(&system(TenantId::DEFAULT, EventKind::Resync)),
        "{got:?}"
    );
    assert!(bus.metrics().overflows >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_gives_resync() {
    let Some((bus, listener_app, admin)) = bus(SignalsConfig::default()).await else {
        return;
    };
    let scope = AccountScope {
        tenant: TenantId::new(),
        account: AccountId::new(),
    };
    let mut sys = bus.subscribe_system().await.expect("subscribe_system");
    let mut acc = bus.subscribe(scope).await.expect("subscribe");
    bus.publish(log(1)).await.expect("publish");
    assert_eq!(next(&mut sys).await, log(1));

    // Обрыв соединения слушателя (как при перезапуске СУБД).
    let killed: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
         WHERE application_name = $1 AND query LIKE 'LISTEN %'",
    )
    .bind(&listener_app)
    .fetch_all(&admin)
    .await
    .expect("terminate");
    assert_eq!(killed, vec![true], "одно соединение слушателя");

    assert_eq!(
        next(&mut sys).await,
        system(TenantId::DEFAULT, EventKind::Resync)
    );
    assert_eq!(
        next(&mut acc).await,
        Event {
            tenant: scope.tenant,
            account: Some(scope.account),
            kind: EventKind::Resync
        }
    );
    assert!(bus.metrics().reconnects >= 1);
    // После переподключения сигналы снова доходят.
    bus.publish(log(2)).await.expect("publish");
    assert_eq!(next(&mut sys).await, log(2));

    // Без шины подписки завершаются.
    drop(bus);
    assert_eq!(
        tokio::time::timeout(WAIT, sys.next()).await.expect("end"),
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passes_contract_suite() {
    let Some((bus, _, _)) = bus(SignalsConfig::default()).await else {
        return;
    };
    tp_testkit::suite::signals(&bus).await;
}

/// Поток контракта: `drop` удаляет подписку, остановка шины завершает поток.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_stream_lifecycle() {
    let Some((bus, _, _)) = bus(SignalsConfig::default()).await else {
        return;
    };
    let bus = Arc::new(bus);
    let dyn_bus: Arc<dyn EventBus> = bus.clone();
    let mut sys = dyn_bus.subscribe_system().await.expect("subscribe_system");
    let acc = dyn_bus
        .subscribe(AccountScope {
            tenant: TenantId::new(),
            account: AccountId::new(),
        })
        .await
        .expect("subscribe");
    assert_eq!(bus.subscribers(), 2);
    drop(acc);
    assert_eq!(bus.subscribers(), 1, "drop потока удаляет подписку");

    dyn_bus.publish(log(3)).await.expect("publish");
    let got = tokio::time::timeout(WAIT, sys.next())
        .await
        .expect("сигнал не пришёл");
    assert_eq!(got, Some(log(3)));

    drop(dyn_bus);
    let bus = Arc::into_inner(bus).expect("единственная ссылка");
    drop(bus);
    assert_eq!(
        tokio::time::timeout(WAIT, sys.next()).await.expect("end"),
        None,
        "остановка шины завершает поток"
    );
}

#[tokio::test]
async fn listener_bypasses_pgbouncer() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let mut cfg = common::dev_pool_config(&url);
    cfg.pgbouncer = true;
    assert!(matches!(
        connect_listener(&cfg, common::secrets()).await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stores_signal_after_commit() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let rec = Arc::new(common::RecordingBus::default());
    let meta = PgMeta::new(pool, MetaConfig::default()).with_bus(rec.clone());
    let n = MetaConfig::default().event_partitions;

    // Новая учётная запись: системные папки — события журнала — LogAppended её раздела.
    let scope = common::account(&meta).await;
    let part = |a: AccountId| {
        system(
            TenantId::DEFAULT,
            EventKind::LogAppended {
                partition: partition_of(a, n),
            },
        )
    };
    assert_eq!(rec.take(), vec![part(scope.account)]);
    meta.create(
        scope,
        NewMailbox {
            parent: None,
            name: "Проекты".into(),
            role: None,
        },
    )
    .await
    .expect("create");
    assert_eq!(rec.take(), vec![part(scope.account)]);
    // Чтение событий не пишет.
    meta.list(scope).await.expect("list");
    assert!(rec.take().is_empty());

    // Вход и выход узла — MembersChanged; продление живой записи — без сигнала.
    let node = NodeInfo::new(NodeId::new(format!("sig-{}", Uuid::now_v7().simple())).expect("id"));
    let ttl = Duration::from_secs(60);
    let members = system(TenantId::DEFAULT, EventKind::MembersChanged);
    meta.heartbeat(&node, ttl).await.expect("heartbeat");
    assert_eq!(rec.take(), vec![members.clone()]);
    meta.heartbeat(&node, ttl).await.expect("renew");
    assert!(rec.take().is_empty());
    meta.leave(&node.id).await.expect("leave");
    assert_eq!(rec.take(), vec![members]);
    meta.leave(&node.id).await.expect("leave again");
    assert!(rec.take().is_empty());
}
