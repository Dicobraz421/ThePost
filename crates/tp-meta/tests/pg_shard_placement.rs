//! META-30: хранилища ящика через справочник шардов — два шарда (две БД одного PostgreSQL), один `PgMeta`.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::acl::AclStore;
use tp_contracts::auth::SendRight;
use tp_contracts::bus::{Event, EventBus, EventKind};
use tp_contracts::meta::{
    AccountScope, ChangeLog, MailboxStore, MessageStore, NewMessage, SpecialUse,
};
use tp_contracts::retention::{Quota, TrashStore};
use tp_contracts::search::{IndexManifestStore, ManifestUpdate, RebuildReason, RebuildRequest};
use tp_contracts::{BlobHash, BlobKey, Gen, MailboxId, ModSeq, Result, TenantId};
use tp_meta::PgMeta;
use tp_meta::pool::DbPool;
use tp_meta::shards::{ReadIntent, ShardId, ShardResolver};
use tp_testkit::infra::MemEventBus;

fn message(scope: AccountScope) -> NewMessage {
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
    NewMessage {
        skeleton: BlobKey {
            tenant: scope.tenant,
            hash: BlobHash(h),
            generation: Gen::INITIAL,
        },
        parts: Vec::new(),
        size: 100,
        received_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts"),
        flags: Default::default(),
        meta: Default::default(),
    }
}

async fn inbox(meta: &PgMeta, scope: AccountScope) -> MailboxId {
    MailboxStore::list(meta, scope)
        .await
        .expect("list")
        .into_iter()
        .find(|m| m.role == Some(SpecialUse::Inbox))
        .expect("inbox")
        .id
}

/// Строк таблицы `table` учётной записи в БД `pool` (все организации).
async fn rows(pool: &DbPool, table: &str, scope: AccountScope) -> i64 {
    let mut tx = tp_meta::schema::begin_system(pool).await.expect("begin");
    let n = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {table} WHERE account_id = $1"
    )))
    .bind(scope.account.as_uuid())
    .fetch_one(&mut *tx)
    .await
    .expect("count");
    tx.commit().await.expect("commit");
    n
}

#[tokio::test]
async fn mailboxes_of_two_shards_through_one_meta() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let org = t.tenant().await;
    let a = t.account_on(org.clone(), ShardId::MAIN).await;
    let b = t.account_on(org, t.shard).await;
    assert_eq!(t.dir.shard_of(a).await, Ok(ShardId::MAIN));
    assert_eq!(t.dir.shard_of(b).await, Ok(t.shard));
    let shards = t.dir.list().await.expect("list");
    assert_eq!(shards[1].site.as_deref(), Some("dc2"));

    // Системные папки — на шарде ящика; доставка одной пачкой в ящики обоих шардов.
    let (ia, ib) = (inbox(&t.meta, a).await, inbox(&t.meta, b).await);
    let ins = t
        .meta
        .insert_batch(
            a.tenant,
            vec![
                (a, ia, message(a)),
                (b, ib, message(b)),
                (a, ia, message(a)),
            ],
        )
        .await
        .expect("insert_batch");
    assert_eq!(ins.len(), 3);
    assert_eq!((ins[0].uid.0, ins[1].uid.0, ins[2].uid.0), (1, 1, 2));
    let single = t.meta.insert(b, ib, message(b)).await.expect("insert");
    assert_eq!(single.uid.0, 2);

    // Данные не пересекаются: ящик `a` — в основной БД, `b` — на шарде.
    let main = t.main.meta.pool();
    for table in [
        "mailboxes",
        "messages",
        "changelog",
        "mailbox_state",
        "events",
    ] {
        assert!(rows(main, table, a).await > 0, "{table}: a в основной БД");
        assert_eq!(rows(main, table, b).await, 0, "{table}: b не в основной БД");
        assert_eq!(
            rows(&t.shard_pool, table, a).await,
            0,
            "{table}: a не на шарде"
        );
        assert!(
            rows(&t.shard_pool, table, b).await > 0,
            "{table}: b на шарде"
        );
    }
    // Каталог — только в основной БД.
    assert_eq!(rows(&t.shard_pool, "accounts", b).await, 0);
    assert_eq!(rows(main, "accounts", b).await, 1);

    assert!(
        MessageStore::get(&t.meta, b, ins[1].id)
            .await
            .expect("get")
            .is_some()
    );
    assert!(
        MessageStore::get(&t.meta, a, ins[1].id)
            .await
            .expect("get")
            .is_none()
    );
    let changes = t
        .meta
        .message_changes(b, None, ModSeq::ZERO, 100)
        .await
        .expect("changes");
    let created: Vec<_> = changes.created.iter().map(|c| c.id).collect();
    assert_eq!(created, vec![ins[1].id, single.id]);
    assert_eq!(
        t.meta.status(b, ib).await.expect("status").messages,
        2,
        "status на шарде"
    );

    // Корзина, квота, делегирование, манифест индекса — на шарде ящика.
    t.meta.to_trash(b, &[ins[1].id]).await.expect("to_trash");
    let trash = TrashStore::list(&t.meta, b, 0, 10).await.expect("trash");
    assert_eq!(trash.len(), 1);
    assert!(trash[0].expires_at > trash[0].deleted_at);
    let usage = Quota::usage(&t.meta, b).await.expect("usage");
    assert_eq!((usage.used_bytes, usage.messages), (200, 2));
    t.meta
        .set_send_delegation(b, "boss@example.test", Some(SendRight::SendAs))
        .await
        .expect("delegation");
    assert_eq!(t.meta.send_delegations(b).await.expect("list").len(), 1);
    assert_eq!(t.meta.send_delegations(a).await.expect("list").len(), 0);
    let v = t
        .meta
        .put_manifest(
            b,
            0,
            ManifestUpdate {
                schema: 1,
                segments: Vec::new(),
                rebuilt: false,
            },
        )
        .await
        .expect("manifest");
    assert_eq!(t.meta.get_manifest(b).await.expect("get").version, v);
    t.meta
        .request_rebuild(
            b,
            RebuildRequest {
                requested_at: OffsetDateTime::now_utc(),
                reason: RebuildReason::Admin,
            },
        )
        .await
        .expect("rebuild");
    assert_eq!(
        t.meta.pending_rebuilds(1, 10).await.expect("pending"),
        vec![b],
        "системная выборка обходит шарды"
    );
    let expired = t
        .meta
        .expired(
            OffsetDateTime::now_utc() + Duration::from_secs(400 * 86_400),
            10,
        )
        .await
        .expect("expired");
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].scope, b);

    t.drop_db().await;
}

#[tokio::test]
async fn directory_signal_drops_cache() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let org = t.tenant().await;
    let b = t.account_on(org, t.shard).await;
    assert_eq!(t.dir.shard_of(b).await, Ok(t.shard));
    assert!(t.dir.pool(t.shard, ReadIntent::Primary).await.is_ok());

    // Перенос в справочнике без сигнала — кэш прежний; сигнал сбрасывает кэш.
    let mut tx = tp_meta::schema::begin(t.main.meta.pool(), b.tenant)
        .await
        .expect("begin");
    sqlx::query("UPDATE accounts SET shard_id = 0 WHERE tenant_id = $1 AND account_id = $2")
        .bind(b.tenant.as_uuid())
        .bind(b.account.as_uuid())
        .execute(&mut *tx)
        .await
        .expect("move");
    tx.commit().await.expect("commit");
    assert_eq!(t.dir.shard_of(b).await, Ok(t.shard));
    let bus = Arc::new(MemEventBus::new(16));
    let follow = t.dir.clone().follow(bus.clone()).await.expect("follow");
    bus.publish(Event {
        tenant: TenantId::DEFAULT,
        account: None,
        kind: EventKind::ConfigReloaded,
    })
    .await
    .expect("publish");
    tokio::time::timeout(Duration::from_secs(10), async {
        while t.dir.shard_of(b).await != Ok(ShardId::MAIN) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("кэш сброшен сигналом");
    follow.abort();

    t.drop_db().await;
}

/// Резолвер с одним шардом, считающий запросы размещения.
struct OneShard {
    lookups: AtomicUsize,
}

#[async_trait]
impl ShardResolver for OneShard {
    async fn shard_of(&self, _: AccountScope) -> Result<ShardId> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        Ok(ShardId::MAIN)
    }

    async fn pool(&self, _: ShardId, _: ReadIntent) -> Result<DbPool> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        Err(tp_contracts::Error::internal("основная БД — пул хранилищ"))
    }

    async fn place(&self, _: TenantId) -> Result<ShardId> {
        Ok(ShardId::MAIN)
    }

    async fn shards(&self) -> Result<Vec<ShardId>> {
        Ok(vec![ShardId::MAIN])
    }
}

#[tokio::test]
async fn single_shard_without_directory_lookups() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let resolver = Arc::new(OneShard {
        lookups: AtomicUsize::new(0),
    });
    let meta = meta.with_shards(resolver.clone());
    let scope = common::account(&meta).await;
    let ib = inbox(&meta, scope).await;
    meta.insert(scope, ib, message(scope))
        .await
        .expect("insert");
    assert_eq!(meta.status(scope, ib).await.expect("status").messages, 1);
    assert_eq!(resolver.lookups.load(Ordering::SeqCst), 0);
}
