//! META-31: журнал событий, очередь и учёт BLOB по шардам — два шарда (две БД одного PostgreSQL).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use sqlx::PgPool;
use time::OffsetDateTime;
use tp_contracts::blob::{Acquire, BlobIndex, Commit};
use tp_contracts::bus::{Event, EventBus, EventKind};
use tp_contracts::events::{EventSink, LoggedEvent, ShardEventLogs};
use tp_contracts::meta::{AccountScope, MailboxStore, MessageStore, NewMessage, SpecialUse};
use tp_contracts::queue::{DsnParams, OutMessage, OutRcpt, QueueStore, RcptStatus};
use tp_contracts::{AccountId, BlobHash, BlobKey, Gen, ModSeq, ShardId, TenantId};
use tp_meta::PgMeta;
use tp_meta::events::{EventConsumer, EventsConfig};
use tp_meta::failover::connect_failover;
use tp_meta::signals::{PgSignalBus, ShardedSignalBus, SignalsConfig, connect_listener};

fn hash() -> BlobHash {
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
    h[16..].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
    BlobHash(h)
}

fn message(scope: AccountScope) -> NewMessage {
    NewMessage {
        skeleton: BlobKey {
            tenant: scope.tenant,
            hash: hash(),
            generation: Gen::INITIAL,
        },
        parts: Vec::new(),
        size: 10,
        received_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts"),
        flags: Default::default(),
        meta: Default::default(),
    }
}

async fn deliver(meta: &PgMeta, scope: AccountScope) {
    let inbox = MailboxStore::list(meta, scope)
        .await
        .expect("list")
        .into_iter()
        .find(|m| m.role == Some(SpecialUse::Inbox))
        .expect("inbox")
        .id;
    meta.insert(scope, inbox, message(scope))
        .await
        .expect("insert");
}

/// Подписчик, запоминающий учётные записи событий.
#[derive(Default)]
struct Seen(Mutex<Vec<AccountId>>);

#[async_trait]
impl EventSink for Seen {
    fn name(&self) -> &str {
        "seen"
    }

    async fn handle(&self, batch: &[LoggedEvent]) -> tp_contracts::Result<()> {
        self.0
            .lock()
            .expect("lock")
            .extend(batch.iter().map(|e| e.event.account));
        Ok(())
    }
}

#[tokio::test]
async fn events_of_both_shards_are_handled() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let org = t.tenant().await;
    let a = t.account_on(org.clone(), ShardId::MAIN).await;
    let b = t.account_on(org, t.shard).await;
    deliver(&t.meta, a).await;
    deliver(&t.meta, b).await;

    let logs = t.meta.shard_logs().await.expect("shard_logs");
    assert_eq!(logs.shards(), vec![ShardId::MAIN, t.shard]);
    let leases = Arc::new(t.main.meta.clone());
    let sink = Seen::default();
    for shard in logs.shards() {
        let log = logs.log(shard).expect("log");
        let consumer = EventConsumer::new(log, leases.clone(), "n1", EventsConfig::default())
            .with_shard(shard);
        let report = consumer.poll(&sink).await.expect("poll");
        assert!(report.failed.is_empty(), "{report:?}");
        assert!(report.handled > 0, "шард {shard}");
        // Лизы разделов шарда — под ключом подписчика шарда.
        let held = consumer.held("seen");
        assert!(!held.is_empty());
        let expected = if shard == ShardId::MAIN {
            "events/seen/".to_owned()
        } else {
            format!("events/seen@{shard}/")
        };
        assert!(
            consumer
                .shard_lease_name("seen", held[0])
                .starts_with(&expected)
        );
        consumer.release_all("seen").await.expect("release");
    }
    let seen = sink.0.lock().expect("lock").clone();
    assert!(
        seen.contains(&a.account) && seen.contains(&b.account),
        "{seen:?}"
    );

    // Очистка журнала — на каждом шарде по позициям его подписчиков.
    assert!(t.meta.purge_events(&["seen"], 1000).await.expect("purge") >= 2);

    t.drop_db().await;
}

fn out(tenant: TenantId, rcpt: &str) -> OutMessage {
    OutMessage {
        tenant,
        blob: BlobKey {
            tenant,
            hash: hash(),
            generation: Gen::INITIAL,
        },
        size: 10,
        from: "s@example.test".into(),
        dsn: DsnParams::default(),
        rcpts: vec![OutRcpt::new(rcpt)],
        next_attempt: OffsetDateTime::now_utc() - Duration::from_secs(60),
    }
}

#[tokio::test]
async fn queue_jobs_of_both_shards_are_claimed() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let (tenant, _, _) = t.tenant().await;
    // Письмо ставит в очередь вызывающий, знающий шард отправителя.
    let main = t.meta.on_shard(ShardId::MAIN).await.expect("main");
    let second = t.meta.on_shard(t.shard).await.expect("shard");
    let q0 = main
        .enqueue(out(tenant, "r0@x.test"))
        .await
        .expect("enqueue");
    let q1 = second
        .enqueue(out(tenant, "r1@x.test"))
        .await
        .expect("enqueue");
    let lease = Duration::from_secs(60);
    let now = OffsetDateTime::now_utc();

    // Обход по очереди: два прохода по одной записи — записи разных шардов.
    let c1 = t.meta.claim(now, 1, lease).await.expect("claim");
    let c2 = t.meta.claim(now, 1, lease).await.expect("claim");
    let mut ids = vec![c1[0].claim.id, c2[0].claim.id];
    ids.sort();
    let mut want = vec![q0, q1];
    want.sort();
    assert_eq!(ids, want);
    assert!(
        t.meta
            .claim(now, 10, lease)
            .await
            .expect("claim")
            .is_empty()
    );

    // Операции захвата — на шарде записи.
    for c in [&c1[0], &c2[0]] {
        t.meta
            .renew(&c.claim, now + Duration::from_secs(120))
            .await
            .expect("renew");
        let addr = c.rcpts[0].rcpt.address.clone();
        let done = t
            .meta
            .complete(
                &c.claim,
                &addr,
                RcptStatus::Delivered {
                    reply: "250 ok".into(),
                },
            )
            .await
            .expect("complete");
        assert!(done);
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM queue")
        .fetch_one(&t.shard_pool)
        .await
        .expect("count");
    assert_eq!(rows, 0);

    t.drop_db().await;
}

async fn state(pool: &PgPool, key: &BlobKey) -> Option<(String, i64)> {
    let mut tx = tp_meta::schema::begin(pool, key.tenant)
        .await
        .expect("begin");
    let row = sqlx::query_as("SELECT state, refcnt FROM blobs WHERE tenant_id = $1 AND hash = $2")
        .bind(key.tenant.as_uuid())
        .bind(key.hash.as_bytes().as_slice())
        .fetch_optional(&mut *tx)
        .await
        .expect("blob");
    tx.commit().await.expect("commit");
    row
}

#[tokio::test]
async fn blob_referenced_on_another_shard_is_kept() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let (tenant, _, _) = t.tenant().await;
    let main = t.meta.on_shard(ShardId::MAIN).await.expect("main");
    let second = t.meta.on_shard(t.shard).await.expect("shard");
    let h = hash();
    let key = BlobKey {
        tenant,
        hash: h,
        generation: Gen::INITIAL,
    };
    // Основная БД: ссылка снята; второй шард: на тот же объект есть ссылка.
    assert_eq!(
        main.acquire(tenant, &h, 1).await.expect("acquire"),
        Acquire::Absent {
            generation: Gen::INITIAL
        }
    );
    assert!(matches!(
        main.commit_new(tenant, &h, Gen::INITIAL, 10, 1).await,
        Ok(Commit::Done { .. })
    ));
    main.release(tenant, &h, Gen::INITIAL, 1)
        .await
        .expect("release");
    assert!(matches!(
        second.commit_new(tenant, &h, Gen::INITIAL, 10, 1).await,
        Ok(Commit::Done { .. })
    ));

    let later = OffsetDateTime::now_utc() + Duration::from_secs(3600);
    assert!(
        !t.meta
            .gc_candidates(later, 100)
            .await
            .expect("candidates")
            .contains(&key),
        "ссылка на втором шарде"
    );
    assert!(!t.meta.mark_deleting(&key, later).await.expect("mark"));
    let main_pool = t.main.meta.pool();
    assert_eq!(state(main_pool, &key).await, Some(("live".into(), 0)));
    assert_eq!(state(&t.shard_pool, &key).await, Some(("live".into(), 1)));
    assert!(t.meta.exists(&key).await.expect("exists"));

    // Ссылок нет ни на одном шарде — объект удаляется.
    second
        .release(tenant, &h, Gen::INITIAL, 1)
        .await
        .expect("release");
    assert!(
        t.meta
            .gc_candidates(later, 100)
            .await
            .expect("candidates")
            .contains(&key)
    );
    assert!(t.meta.mark_deleting(&key, later).await.expect("mark"));
    assert!(t.meta.finalize_delete(&key).await.expect("finalize"));
    assert!(!t.meta.exists(&key).await.expect("exists"));

    // Шард без строки получает надгробие: загрузка на нём после удаления — новое поколение.
    let h2 = hash();
    let key2 = BlobKey {
        tenant,
        hash: h2,
        generation: Gen::INITIAL,
    };
    main.acquire(tenant, &h2, 1).await.expect("acquire");
    main.commit_new(tenant, &h2, Gen::INITIAL, 10, 1)
        .await
        .expect("commit");
    main.release(tenant, &h2, Gen::INITIAL, 1)
        .await
        .expect("release");
    assert!(t.meta.mark_deleting(&key2, later).await.expect("mark"));
    assert_eq!(
        second.acquire(tenant, &h2, 1).await.expect("acquire"),
        Acquire::Absent {
            generation: Gen(Gen::INITIAL.0 + 1)
        }
    );
    assert!(t.meta.finalize_delete(&key2).await.expect("finalize"));

    t.drop_db().await;
}

#[tokio::test]
async fn signals_from_every_shard() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let org = t.tenant().await;
    let b = t.account_on(org, t.shard).await;
    let cfg = common::dev_pool_config(&t.main_url());
    let secrets = common::secrets();
    let main = PgSignalBus::start(
        connect_failover(&cfg, secrets.clone())
            .await
            .expect("publish"),
        connect_listener(&cfg, secrets.clone())
            .await
            .expect("listen"),
        SignalsConfig::default(),
    )
    .expect("main bus");
    let bus = ShardedSignalBus::start(main, t.dir.clone(), secrets, SignalsConfig::default())
        .await
        .expect("sharded bus");
    assert_eq!(bus.shards(), vec![ShardId::MAIN, t.shard]);

    let mut sub = bus.subscribe(b).await.expect("subscribe");
    let changed = Event {
        tenant: b.tenant,
        account: Some(b.account),
        kind: EventKind::MailboxChanged {
            mailbox: tp_contracts::MailboxId::new(),
            modseq: ModSeq(1),
        },
    };
    bus.publish(changed.clone()).await.expect("publish");
    let got = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match sub.next().await {
                Some(e) if e.kind == EventKind::Resync => continue,
                other => return other,
            }
        }
    })
    .await
    .expect("сигнал учётной записи шарда");
    assert_eq!(got, Some(changed));

    // Слушатель — на primary шарда: сообщение NOTIFY в БД шарда доходит до подписки (сверка всем).
    sqlx::query("SELECT pg_notify('tp_signals', '{\"t\":0,\"r\":true}')")
        .execute(&t.shard_pool)
        .await
        .expect("notify");
    let resync = tokio::time::timeout(Duration::from_secs(10), sub.next())
        .await
        .expect("сверка из шарда");
    assert_eq!(resync.map(|e| e.kind), Some(EventKind::Resync));

    drop(sub);
    drop(bus);
    t.drop_db().await;
}
