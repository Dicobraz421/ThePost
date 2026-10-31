//! META-09: QueueStore на PostgreSQL.

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tp_contracts::queue::{
    DsnNotify, DsnParams, DsnRet, OutMessage, OutRcpt, QueueStore, RcptStatus,
};
use tp_contracts::{BlobHash, BlobKey, Error, Gen, QueueId, TenantId};
use tp_meta::PgMeta;

const LEASE: Duration = Duration::from_secs(60);

/// Тестовые часы: сценарии захвата идут на своей чистой БД, записи других тестов не видны.
fn clock() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(2_000_000_000).expect("ts")
}

fn blob(tenant: TenantId) -> BlobKey {
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
    BlobKey {
        tenant,
        hash: BlobHash(h),
        generation: Gen::INITIAL,
    }
}

fn message(tenant: TenantId, rcpts: &[&str], at: OffsetDateTime) -> OutMessage {
    OutMessage {
        tenant,
        blob: blob(tenant),
        size: 1234,
        from: "a@example.org".into(),
        dsn: DsnParams {
            envid: Some("ENV1".into()),
            ret: Some(DsnRet::Headers),
        },
        rcpts: rcpts.iter().map(|r| OutRcpt::new(*r)).collect(),
        next_attempt: at,
    }
}

/// Захватывает доступные записи и оставляет только `ids`.
async fn claim_own(
    meta: &PgMeta,
    now: OffsetDateTime,
    ids: &HashSet<QueueId>,
) -> Vec<tp_contracts::queue::Claimed> {
    meta.claim(now, 10_000, LEASE)
        .await
        .expect("claim")
        .into_iter()
        .filter(|c| ids.contains(&c.claim.id))
        .collect()
}

#[tokio::test]
async fn enqueue_validation() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let meta = &meta;
    let tenant = common::tenant(meta.pool()).await;
    let now = clock();
    assert!(matches!(
        meta.enqueue(message(tenant, &[], now)).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        meta.enqueue(message(tenant, &["b@x.test", "b@x.test"], now))
            .await,
        Err(Error::Invalid(_))
    ));
    let mut foreign = message(tenant, &["b@x.test"], now);
    foreign.blob.tenant = TenantId::new();
    assert!(matches!(
        meta.enqueue(foreign).await,
        Err(Error::Invalid(_))
    ));
}

async fn claim_fencing_and_completion(meta: &PgMeta) {
    let tenant = common::tenant(meta.pool()).await;
    let now = clock() + time::Duration::days(1);
    let mut msg = message(tenant, &["b@x.test", "c@x.test"], now);
    msg.rcpts[1].notify = Some(DsnNotify {
        failure: true,
        delay: true,
        ..DsnNotify::default()
    });
    msg.rcpts[1].orcpt = Some("rfc822;c@x.test".into());
    let id = meta.enqueue(msg.clone()).await.expect("enqueue");
    let own: HashSet<QueueId> = [id].into();

    // До срока попытки запись не выдаётся.
    assert!(
        claim_own(meta, now - time::Duration::seconds(1), &own)
            .await
            .is_empty()
    );
    let first = claim_own(meta, now, &own).await;
    assert_eq!(first.len(), 1);
    let c1 = &first[0];
    assert_eq!(c1.tenant, tenant);
    assert_eq!(c1.blob, msg.blob);
    assert_eq!(c1.size, 1234);
    assert_eq!(c1.from, msg.from);
    assert_eq!(c1.dsn, msg.dsn);
    assert_eq!(c1.until, now + LEASE);
    let rcpts: Vec<_> = c1.rcpts.iter().map(|r| r.rcpt.clone()).collect();
    assert_eq!(rcpts, msg.rcpts);
    assert!(c1.rcpts.iter().all(|r| r.attempts == 0));

    // Действующая лиза: повторно не выдаётся.
    assert!(
        claim_own(meta, now + time::Duration::seconds(30), &own)
            .await
            .is_empty()
    );
    // Лиза истекла — запись берёт другой узел, номер захвата растёт; старый захват устарел.
    let later = now + time::Duration::seconds(61);
    let second = claim_own(meta, later, &own).await;
    assert_eq!(second.len(), 1);
    let c2 = second[0].claim;
    assert!(c2.fencing > c1.claim.fencing);
    // Получатели не завершены до истечения лизы — без увеличения attempts.
    assert!(second[0].rcpts.iter().all(|r| r.attempts == 0));
    assert_eq!(
        meta.renew(&c1.claim, later + LEASE).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        meta.reschedule(&c1.claim, "b@x.test", later, "451").await,
        Err(Error::Conflict)
    );
    assert_eq!(
        meta.complete(
            &c1.claim,
            "b@x.test",
            RcptStatus::Delivered {
                reply: "250".into()
            }
        )
        .await,
        Err(Error::Conflict)
    );
    meta.renew(&c2, later + LEASE * 2).await.expect("renew");

    // Неизвестный получатель — NotFound.
    assert_eq!(
        meta.reschedule(&c2, "z@x.test", later, "451").await,
        Err(Error::NotFound)
    );
    assert!(
        !meta
            .complete(
                &c2,
                "b@x.test",
                RcptStatus::Delivered {
                    reply: "250 ok".into()
                }
            )
            .await
            .expect("complete b")
    );
    // Завершённый получатель — NotFound.
    assert_eq!(
        meta.complete(&c2, "b@x.test", RcptStatus::Failed { reply: "x".into() })
            .await,
        Err(Error::NotFound)
    );
    assert!(
        meta.complete(
            &c2,
            "c@x.test",
            RcptStatus::Failed {
                reply: "550 5.1.1".into()
            }
        )
        .await
        .expect("complete c")
    );
    // Запись удалена: захват больше недействителен.
    assert_eq!(meta.renew(&c2, later).await, Err(Error::Conflict));
    assert!(
        claim_own(meta, later + time::Duration::days(1), &own)
            .await
            .is_empty()
    );
}

async fn retry_by_time(meta: &PgMeta) {
    let tenant = common::tenant(meta.pool()).await;
    let now = clock() + time::Duration::days(2);
    let id = meta
        .enqueue(message(tenant, &["b@x.test", "c@x.test"], now))
        .await
        .expect("enqueue");
    let own: HashSet<QueueId> = [id].into();
    let c = claim_own(meta, now, &own).await.remove(0).claim;
    let retry_b = now + time::Duration::minutes(10);
    let retry_c = now + time::Duration::minutes(30);
    meta.reschedule(&c, "b@x.test", retry_b, "451 4.7.1 greylisted")
        .await
        .expect("reschedule b");
    meta.reschedule(&c, "c@x.test", retry_c, "421 busy")
        .await
        .expect("reschedule c");

    // Лиза истекла, но срок повтора ещё не наступил.
    let t = now + time::Duration::minutes(5);
    assert!(claim_own(meta, t, &own).await.is_empty());
    // Наступил срок только для b.
    let got = claim_own(meta, retry_b, &own).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].rcpts.len(), 1);
    let b = &got[0].rcpts[0];
    assert_eq!(b.rcpt.address, "b@x.test");
    assert_eq!(b.attempts, 1);
    assert_eq!(b.last_error.as_deref(), Some("451 4.7.1 greylisted"));
    assert!(
        !meta
            .complete(
                &got[0].claim,
                "b@x.test",
                RcptStatus::Delivered {
                    reply: "250".into()
                }
            )
            .await
            .expect("complete")
    );
    // Затем c — после истечения лизы предыдущего захвата.
    let got = claim_own(meta, retry_c + LEASE, &own).await;
    assert_eq!(got.len(), 1);
    let rc: Vec<_> = got[0]
        .rcpts
        .iter()
        .map(|r| r.rcpt.address.as_str())
        .collect();
    assert_eq!(rc, ["c@x.test"]);
    assert_eq!(got[0].rcpts[0].attempts, 1);
    assert!(
        meta.complete(
            &got[0].claim,
            "c@x.test",
            RcptStatus::Delivered {
                reply: "250".into()
            }
        )
        .await
        .expect("complete c")
    );
}

async fn concurrent_claim_once(meta: Arc<PgMeta>) {
    let tenant = common::tenant(meta.pool()).await;
    let now = clock() + time::Duration::days(3);
    let mut own = HashSet::new();
    for i in 0..40 {
        let at = now - time::Duration::seconds(i);
        own.insert(
            meta.enqueue(message(tenant, &["b@x.test"], at))
                .await
                .expect("enqueue"),
        );
    }
    let own = Arc::new(own);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let meta = Arc::clone(&meta);
        let own = Arc::clone(&own);
        tasks.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            loop {
                let got: Vec<QueueId> = meta
                    .claim(now, 3, LEASE)
                    .await
                    .expect("claim")
                    .into_iter()
                    .map(|c| c.claim.id)
                    .filter(|id| own.contains(id))
                    .collect();
                if got.is_empty() {
                    break;
                }
                mine.extend(got);
            }
            mine
        }));
    }
    let mut all = Vec::new();
    for t in tasks {
        all.extend(t.await.expect("join"));
    }
    let unique: HashSet<QueueId> = all.iter().copied().collect();
    assert_eq!(all.len(), unique.len(), "запись выдана дважды");
    assert_eq!(unique, *own);
}

/// Сценарии захвата на одной чистой БД (claim видит записи всех организаций): по порядку, каждый
/// завершает свои записи или использует более поздние часы.
#[tokio::test]
async fn claim_scenarios() {
    let Some(db) = common::fresh_meta().await else {
        return;
    };
    claim_fencing_and_completion(&db.meta).await;
    retry_by_time(&db.meta).await;
    concurrent_claim_once(Arc::new(db.meta.clone())).await;
    db.drop_db().await;
}
