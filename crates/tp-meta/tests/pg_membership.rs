//! Членство узлов на PostgreSQL (META-16, CONTRACTS.md#Membership).

mod common;

use std::time::Duration;

use tp_contracts::Error;
use tp_contracts::membership::{Membership, NodeId};
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
        meta.heartbeat(n, long).await.expect("heartbeat");
    }
    assert_eq!(live(&meta, &prefix).await, ids);
    // Повторное продление — та же запись.
    meta.heartbeat(&ids[0], long).await.expect("heartbeat");
    assert_eq!(live(&meta, &prefix).await, ids);

    // Запись с коротким сроком истекает по часам СУБД.
    let short = Duration::from_millis(300);
    meta.heartbeat(&ids[1], short).await.expect("heartbeat");
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
    meta.heartbeat(&ids[1], long).await.expect("heartbeat");
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
    let roles = vec!["imap".to_owned(), "indexer".to_owned()];
    meta.heartbeat_roles(n, &roles, ttl).await.expect("roles");
    // Продление без ролей их не меняет.
    meta.heartbeat(n, ttl).await.expect("heartbeat");
    let rec = meta
        .nodes()
        .await
        .expect("nodes")
        .into_iter()
        .find(|r| &r.node == n)
        .expect("record");
    assert_eq!(rec.roles, roles);
    assert!(rec.expires_at > rec.joined_at);

    assert!(matches!(
        meta.heartbeat(n, Duration::ZERO).await,
        Err(Error::Invalid(_))
    ));
    meta.leave(n).await.expect("cleanup");
}
