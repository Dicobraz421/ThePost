use std::sync::Arc;
use std::time::Duration;

use tp_contracts::abuse::{AbuseBlocks, BlockSubject};
use tp_contracts::bus::{Event, EventKind};
use tp_contracts::{AccountId, TenantId};
use tp_testkit::{Clock, ManualClock, MemAbuseBlocks, MemEventBus};

use super::*;

fn ip(s: &str) -> IpAddr {
    s.parse().expect("ip")
}

fn from_ip(s: &str) -> Attempt {
    Attempt {
        ip: Some(ip(s)),
        account: None,
    }
}

fn node(cfg: &AbuseConfig, store: Arc<dyn AbuseBlocks>, clock: &ManualClock) -> AbuseGuard {
    let c = clock.clone();
    AbuseGuard::with_clock(
        cfg.clone(),
        store,
        Arc::new(move || SystemTime::from(c.now())),
    )
}

/// Кластер из двух узлов над общим хранилищем и шиной; `silent` — то же хранилище без сигналов.
struct Cluster {
    clock: ManualClock,
    bus: MemEventBus,
    silent: MemAbuseBlocks,
    store: MemAbuseBlocks,
    a: AbuseGuard,
    b: AbuseGuard,
}

async fn cluster(cfg: AbuseConfig) -> Cluster {
    let clock = ManualClock::default();
    let bus = MemEventBus::default();
    let silent = MemAbuseBlocks::new(clock.shared());
    let store = silent.clone().with_bus(Arc::new(bus.clone()));
    let a = node(&cfg, Arc::new(store.clone()), &clock);
    let b = node(&cfg, Arc::new(store.clone()), &clock);
    for g in [&a, &b] {
        let (g, bus) = (g.clone(), Arc::new(bus.clone()));
        tokio::spawn(async move { g.listen(bus).await });
    }
    wait_subscribers(&bus, 2).await;
    Cluster {
        clock,
        bus,
        silent,
        store,
        a,
        b,
    }
}

async fn wait_subscribers(bus: &MemEventBus, n: usize) {
    for _ in 0..500 {
        if bus.subscribers() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no subscribers");
}

/// Ждёт (до 5 с), пока проверка на узле `g` не даст `blocked`.
async fn eventually(g: &AbuseGuard, tenant: TenantId, attempt: &Attempt, blocked: bool) {
    for _ in 0..500 {
        if g.check(tenant, attempt).await.expect("check").is_some() == blocked {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected blocked = {blocked}");
}

#[tokio::test]
async fn block_on_one_node_applies_on_other() {
    let cfg = AbuseConfig {
        ip_failures: 3,
        ..AbuseConfig::default()
    };
    let c = cluster(cfg.clone()).await;
    let tenant = TenantId::new();
    let bad = from_ip("192.0.2.10");
    // Узел B загрузил пустой набор до блокировки — дальше узнаёт только по сигналу.
    assert_eq!(c.b.check(tenant, &bad).await.expect("check"), None);
    assert_eq!(c.a.failure(tenant, &bad).await.expect("fail"), None);
    assert_eq!(c.a.failure(tenant, &bad).await.expect("fail"), None);
    let until =
        c.a.failure(tenant, &bad)
            .await
            .expect("fail")
            .expect("blocked");
    assert_eq!(until, SystemTime::from(c.clock.now()) + cfg.block_for);
    assert_eq!(c.a.check(tenant, &bad).await.expect("check"), Some(until));
    eventually(&c.b, tenant, &bad, true).await;
    assert_eq!(c.b.check(tenant, &bad).await.expect("check"), Some(until));
    // Соседний адрес и другая организация не затронуты.
    assert_eq!(
        c.b.check(tenant, &from_ip("192.0.2.11"))
            .await
            .expect("check"),
        None
    );
    assert_eq!(c.b.check(TenantId::new(), &bad).await.expect("check"), None);
    // По сроку блокировка снимается на всех узлах без сигнала.
    c.clock.advance(cfg.block_for + Duration::from_secs(1));
    assert_eq!(c.a.check(tenant, &bad).await.expect("check"), None);
    assert_eq!(c.b.check(tenant, &bad).await.expect("check"), None);
}

#[tokio::test]
async fn unblock_signal_clears_other_node() {
    let c = cluster(AbuseConfig::default()).await;
    let tenant = TenantId::new();
    let bad = from_ip("198.51.100.7");
    let net = BlockSubject::net(ip("198.51.100.0"), 24).expect("net");
    assert_eq!(c.b.check(tenant, &bad).await.expect("check"), None);
    let until = SystemTime::from(c.clock.now()) + Duration::from_secs(600);
    c.store
        .block(tenant, &net, until.into(), "admin")
        .await
        .expect("block");
    eventually(&c.b, tenant, &bad, true).await;
    eventually(&c.a, tenant, &bad, true).await;
    c.store.unblock(tenant, &net).await.expect("unblock");
    eventually(&c.b, tenant, &bad, false).await;
    eventually(&c.a, tenant, &bad, false).await;
}

#[tokio::test]
async fn resync_after_lost_signals() {
    let c = cluster(AbuseConfig::default()).await;
    let tenant = TenantId::new();
    let account = AccountId::new();
    let who = Attempt {
        ip: Some(ip("203.0.113.5")),
        account: Some(account),
    };
    assert_eq!(c.b.check(tenant, &who).await.expect("check"), None);
    // Блокировка без сигнала (сигнал потерян): узел её не видит до сверки.
    let until = SystemTime::from(c.clock.now()) + Duration::from_secs(600);
    c.silent
        .block(
            tenant,
            &BlockSubject::Account(account),
            until.into(),
            "lost",
        )
        .await
        .expect("block");
    assert_eq!(c.b.check(tenant, &who).await.expect("check"), None);
    // Потеря связи: переподписка и сверка.
    c.bus.disconnect();
    eventually(&c.b, tenant, &who, true).await;

    // Снятие без сигнала и `Resync` в потоке.
    c.silent
        .unblock(tenant, &BlockSubject::Account(account))
        .await
        .expect("unblock");
    assert!(c.b.check(tenant, &who).await.expect("check").is_some());
    c.b.apply(&Event {
        tenant: TenantId::DEFAULT,
        account: None,
        kind: EventKind::Resync,
    })
    .await;
    assert_eq!(c.b.check(tenant, &who).await.expect("check"), None);
}

#[tokio::test]
async fn net_and_account_thresholds() {
    let cfg = AbuseConfig {
        ip_failures: 0,
        net_failures: 3,
        account_failures: 2,
        ..AbuseConfig::default()
    };
    let clock = ManualClock::default();
    let store = MemAbuseBlocks::new(clock.shared());
    let g = node(&cfg, Arc::new(store.clone()), &clock);
    let tenant = TenantId::new();
    // Подсеть: неудачи с разных адресов /24 складываются.
    for last in 1..=2 {
        let a = from_ip(&format!("10.0.0.{last}"));
        assert_eq!(g.failure(tenant, &a).await.expect("fail"), None);
    }
    assert!(
        g.failure(tenant, &from_ip("10.0.0.3"))
            .await
            .expect("fail")
            .is_some()
    );
    assert!(
        g.check(tenant, &from_ip("10.0.0.200"))
            .await
            .expect("check")
            .is_some()
    );
    assert_eq!(
        g.check(tenant, &from_ip("10.0.1.1")).await.expect("check"),
        None
    );
    let net = BlockSubject::net(ip("10.0.0.0"), 24).expect("net");
    assert!(
        store
            .blocked(tenant, &net)
            .await
            .expect("blocked")
            .is_some()
    );

    // Учётная запись: успех сбрасывает счётчик.
    let account = AccountId::new();
    let who = Attempt {
        ip: None,
        account: Some(account),
    };
    assert_eq!(g.failure(tenant, &who).await.expect("fail"), None);
    g.success(tenant, &who).await;
    assert_eq!(g.failure(tenant, &who).await.expect("fail"), None);
    assert!(g.failure(tenant, &who).await.expect("fail").is_some());
    assert!(g.check(tenant, &who).await.expect("check").is_some());
    // IPv6 — подсеть /64.
    let v6 = from_ip("2001:db8:1:2::1");
    for _ in 0..3 {
        g.failure(tenant, &v6).await.expect("fail");
    }
    assert!(
        g.check(tenant, &from_ip("2001:db8:1:2:ffff::9"))
            .await
            .expect("check")
            .is_some()
    );
    assert_eq!(
        g.check(tenant, &from_ip("2001:db8:1:3::1"))
            .await
            .expect("check"),
        None
    );
}

#[tokio::test]
async fn store_errors_propagate() {
    let clock = ManualClock::default();
    let store = MemAbuseBlocks::new(clock.shared());
    let g = node(&AbuseConfig::default(), Arc::new(store.clone()), &clock);
    let tenant = TenantId::new();
    store.fail_next(tp_contracts::Error::Unavailable { retry_after: None });
    assert!(g.check(tenant, &from_ip("192.0.2.1")).await.is_err());
    // Следующая проверка перечитывает набор.
    assert_eq!(
        g.check(tenant, &from_ip("192.0.2.1")).await.expect("check"),
        None
    );
}

#[test]
fn config_check() {
    assert!(AbuseConfig::default().check().is_ok());
    let bad = AbuseConfig {
        net_prefix_v4: 33,
        ..AbuseConfig::default()
    };
    assert!(bad.check().is_err());
    let bad = AbuseConfig {
        window: Duration::ZERO,
        ..AbuseConfig::default()
    };
    assert!(bad.check().is_err());
}
