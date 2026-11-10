//! Членство узлов на PostgreSQL (META-16, CONTRACTS.md#Membership).

mod common;

use std::sync::Arc;
use std::time::Duration;

use tp_contracts::bus::{Event, EventKind};
use tp_contracts::membership::{Membership, NodeId, NodeInfo, SiteId};
use tp_contracts::role::Role;
use tp_contracts::{Error, TenantId};
use tp_meta::{MetaConfig, PgMeta};
use tp_testkit::suite::{self, RealTime};
use uuid::Uuid;

/// Узлы теста: общий префикс, чтобы не видеть узлы параллельных тестов.
fn nodes(n: usize) -> (String, Vec<NodeId>) {
    let prefix = format!("t{}", Uuid::now_v7().simple());
    let ids = (0..n)
        .map(|i| NodeId::new(format!("{prefix}-{i}")).expect("node"))
        .collect();
    (prefix, ids)
}

async fn live(m: &dyn Membership, prefix: &str) -> Vec<NodeId> {
    m.live()
        .await
        .expect("live")
        .into_iter()
        .filter(|n| n.as_str().starts_with(prefix))
        .collect()
}

#[tokio::test]
async fn heartbeat_expiry_and_leave() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let (prefix, ids) = nodes(3);
    let long = Duration::from_secs(60);
    // Порядок продлений не важен: состав — по возрастанию.
    for n in ids.iter().rev() {
        meta.heartbeat(&NodeInfo::new(n.clone()), long)
            .await
            .expect("heartbeat");
    }
    assert_eq!(live(&meta, &prefix).await, ids);
    // Повторное продление — та же запись.
    meta.heartbeat(&NodeInfo::new(ids[0].clone()), long)
        .await
        .expect("heartbeat");
    assert_eq!(live(&meta, &prefix).await, ids);

    // Запись с коротким сроком истекает по часам СУБД.
    let short = Duration::from_millis(300);
    meta.heartbeat(&NodeInfo::new(ids[1].clone()), short)
        .await
        .expect("heartbeat");
    let joined = |records: &[tp_meta::membership::NodeRecord], n: &NodeId| {
        records
            .iter()
            .find(|r| &r.node == n)
            .map(|r| r.joined_at)
            .expect("record")
    };
    let first = joined(&meta.nodes().await.expect("nodes"), &ids[1]);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(live(&meta, &prefix).await, [ids[0].clone(), ids[2].clone()]);
    // Возвращение узла — новая запись.
    meta.heartbeat(&NodeInfo::new(ids[1].clone()), long)
        .await
        .expect("heartbeat");
    assert_eq!(live(&meta, &prefix).await, ids);
    assert!(joined(&meta.nodes().await.expect("nodes"), &ids[1]) > first);

    // Выход — сразу; повторный выход и выход неизвестного узла — не ошибка.
    meta.leave(&ids[2]).await.expect("leave");
    meta.leave(&ids[2]).await.expect("leave again");
    assert_eq!(live(&meta, &prefix).await, ids[..2]);
    for n in &ids {
        meta.leave(n).await.expect("cleanup");
    }
    assert!(live(&meta, &prefix).await.is_empty());
}

#[tokio::test]
async fn roles_and_invalid_ttl() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let (_, ids) = nodes(1);
    let n = &ids[0];
    let ttl = Duration::from_secs(60);
    // Запись узла (META-27): роли, адрес, площадка; heartbeat_roles меняет только роли.
    let mut info = NodeInfo::new(n.clone());
    info.roles = [Role::Imap, Role::Search].into_iter().collect();
    info.addr = "https://node.internal:7443".to_owned();
    info.site = SiteId::new("dc-2").expect("site");
    meta.heartbeat(&info, ttl).await.expect("heartbeat");
    let roles = vec![
        "imap".to_owned(),
        "indexer".to_owned(),
        "mod-archive".to_owned(),
    ];
    meta.heartbeat_roles(n, &roles, ttl).await.expect("roles");
    let rec = meta
        .nodes()
        .await
        .expect("nodes")
        .into_iter()
        .find(|r| &r.node == n)
        .expect("record");
    assert_eq!(rec.roles, roles);
    assert_eq!(rec.addr, info.addr);
    assert_eq!(rec.site, info.site);
    assert!(rec.expires_at > rec.joined_at);
    // В live_info — только роли ядра (роль модуля не входит).
    let live = meta.live_info().await.expect("live_info");
    let got = live.iter().find(|i| &i.id == n).expect("live");
    assert_eq!(got.roles, [Role::Imap, Role::Indexer].into_iter().collect());
    assert_eq!(got.addr, info.addr);

    let mut bad = info.clone();
    bad.addr = "https://a b".to_owned();
    assert!(matches!(
        meta.heartbeat(&bad, ttl).await,
        Err(Error::Invalid(_))
    ));

    assert!(matches!(
        meta.heartbeat(&NodeInfo::new(n.clone()), Duration::ZERO)
            .await,
        Err(Error::Invalid(_))
    ));
    meta.leave(n).await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_membership() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let ttl = Duration::from_secs(2);
    suite::membership(&meta, ttl, &RealTime).await;
    suite::membership_info(&meta, ttl, &RealTime).await;
}

#[tokio::test]
async fn members_changed_on_change() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let rec = Arc::new(common::RecordingBus::default());
    let meta = PgMeta::new(pool, MetaConfig::default()).with_bus(rec.clone());
    let (_, ids) = nodes(1);
    let ttl = Duration::from_secs(60);
    let members = Event {
        tenant: TenantId::DEFAULT,
        account: None,
        kind: EventKind::MembersChanged,
    };
    let mut info = NodeInfo::new(ids[0].clone());
    meta.heartbeat(&info, ttl).await.expect("join");
    assert_eq!(rec.take(), vec![members.clone()], "вход");
    meta.heartbeat(&info, ttl).await.expect("renew");
    assert!(rec.take().is_empty(), "продление без изменений");
    info.roles.insert(Role::Search);
    meta.heartbeat(&info, ttl).await.expect("roles");
    assert_eq!(rec.take(), vec![members.clone()], "роли");
    info.addr = "https://n.internal:7443".to_owned();
    meta.heartbeat(&info, ttl).await.expect("addr");
    assert_eq!(rec.take(), vec![members.clone()], "адрес");
    info.site = SiteId::new("dc-9").expect("site");
    meta.heartbeat(&info, ttl).await.expect("site");
    assert_eq!(rec.take(), vec![members.clone()], "площадка");
    meta.heartbeat(&info, ttl).await.expect("renew");
    assert!(rec.take().is_empty());
    // Истёкшая запись: возвращение — вход.
    meta.heartbeat(&info, Duration::from_millis(100))
        .await
        .expect("short");
    tokio::time::sleep(Duration::from_millis(300)).await;
    meta.leave(&info.id).await.expect("leave expired");
    assert!(rec.take().is_empty(), "выход истёкшей записи — без сигнала");
    meta.heartbeat(&info, ttl).await.expect("back");
    assert_eq!(rec.take(), vec![members.clone()]);
    meta.leave(&info.id).await.expect("leave");
    assert_eq!(rec.take(), vec![members]);
}
