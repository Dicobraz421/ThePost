//! LeaseStore на PostgreSQL (META-05).

mod common;

use std::time::Duration;

use tp_contracts::lease::LeaseStore;
use uuid::Uuid;

fn name() -> String {
    format!("test/{}", Uuid::now_v7())
}

#[tokio::test]
async fn second_holder_waits_for_expiry() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let name = name();
    let ttl = Duration::from_millis(400);
    let a = meta
        .try_acquire(&name, "a", ttl)
        .await
        .expect("try_acquire")
        .expect("a");
    assert_eq!(a.holder, "a");
    assert!(
        meta.try_acquire(&name, "b", ttl)
            .await
            .expect("try_acquire")
            .is_none()
    );

    // Повторный захват тем же держателем продлевает лизу без нового номера.
    let a2 = meta
        .try_acquire(&name, "a", ttl)
        .await
        .expect("try_acquire")
        .expect("a2");
    assert_eq!(a2.fencing, a.fencing);
    assert!(a2.expires_at >= a.expires_at);
    assert!(
        meta.try_acquire(&name, "b", ttl)
            .await
            .expect("try_acquire")
            .is_none()
    );

    tokio::time::sleep(Duration::from_millis(600)).await;
    let b = meta
        .try_acquire(&name, "b", ttl)
        .await
        .expect("try_acquire")
        .expect("b");
    assert_eq!(b.holder, "b");
    assert!(b.fencing > a.fencing);

    // Потерянную лизу не продлить, её освобождение — не ошибка и не трогает нового держателя.
    assert!(meta.renew(&a, ttl).await.expect("renew").is_none());
    meta.release(&a).await.expect("release");
    assert!(
        meta.try_acquire(&name, "a", ttl)
            .await
            .expect("try_acquire")
            .is_none()
    );
}

#[tokio::test]
async fn renew_and_release() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let name = name();
    let short = Duration::from_millis(300);
    let a = meta
        .try_acquire(&name, "a", short)
        .await
        .expect("try_acquire")
        .expect("a");
    let long = Duration::from_secs(60);
    let r = meta.renew(&a, long).await.expect("renew").expect("renew");
    assert_eq!(r.fencing, a.fencing);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        meta.try_acquire(&name, "b", long)
            .await
            .expect("try_acquire")
            .is_none()
    );

    meta.release(&r).await.expect("release");
    let b = meta
        .try_acquire(&name, "b", long)
        .await
        .expect("try_acquire")
        .expect("b");
    assert_eq!(b.fencing, a.fencing + 1);
}

#[tokio::test]
async fn concurrent_acquire_single_winner() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let name = name();
    let tasks: Vec<_> = (0..16)
        .map(|i| {
            let meta = meta.clone();
            let name = name.clone();
            tokio::spawn(async move {
                meta.try_acquire(&name, &format!("h{i}"), Duration::from_secs(30))
                    .await
                    .expect("from_secs")
            })
        })
        .collect();
    let mut won = 0;
    for t in tasks {
        if t.await.expect("collect").is_some() {
            won += 1;
        }
    }
    assert_eq!(won, 1);
}
