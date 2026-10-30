//! Справочник шардов (META-08).

mod common;

use std::sync::Arc;

use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Error};
use tp_meta::pool::PoolConfig;
use tp_meta::schema::{self, db_err};
use tp_meta::shards::{ShardDirectory, ShardId, ShardResolver};
use uuid::Uuid;

/// Вызывающий код: знает только резолвер.
async fn count_mailboxes(resolver: &dyn ShardResolver, scope: AccountScope) -> i64 {
    let pool = resolver.pool_for(scope).await.expect("pool_for");
    let mut tx = schema::begin(&pool, scope.tenant).await.expect("begin");
    let n = sqlx::query_scalar("SELECT count(*) FROM mailboxes WHERE account_id = $1")
        .bind(scope.account.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)
        .expect("count");
    tx.commit().await.expect("commit");
    n
}

async fn set_shard(meta: &tp_meta::PgMeta, scope: AccountScope, shard: ShardId) {
    let mut tx = schema::begin(meta.pool(), scope.tenant)
        .await
        .expect("begin");
    sqlx::query("UPDATE accounts SET shard_id = $3 WHERE tenant_id = $1 AND account_id = $2")
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(shard.0)
        .execute(&mut *tx)
        .await
        .expect("update");
    tx.commit().await.expect("commit");
}

#[tokio::test]
async fn resolve_known_account() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let dir = ShardDirectory::new(meta.pool().clone(), PoolConfig::default());
    assert_eq!(dir.shard_of(scope).await.expect("shard_of"), ShardId::LOCAL);
    assert!(count_mailboxes(&dir, scope).await > 0);
    assert_eq!(
        dir.place(scope.tenant).await.expect("place"),
        ShardId::LOCAL
    );
    let local = dir.list().await.expect("list");
    assert_eq!(local[0].id, ShardId::LOCAL);
    assert!(local[0].dsn.is_none());

    let missing = AccountScope {
        tenant: scope.tenant,
        account: AccountId::new(),
    };
    assert_eq!(dir.shard_of(missing).await, Err(Error::NotFound));
    assert!(matches!(dir.pool(ShardId(-1)).await, Err(Error::NotFound)));
}

#[tokio::test]
async fn new_shard_without_caller_changes() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let marker = format!("shard-{}", Uuid::now_v7().simple());
    let resolver: Arc<dyn ShardResolver> = Arc::new(ShardDirectory::new(
        meta.pool().clone(),
        PoolConfig {
            application_name: marker.clone(),
            ..PoolConfig::default()
        },
    ));
    let before = count_mailboxes(resolver.as_ref(), scope).await;

    // Второй шард — та же БД под другим адресом; учётная запись переключается в справочнике.
    let id = ShardId(i32::try_from(Uuid::now_v7().as_u128() % 1_000_000_000).expect("as_u128") + 1);
    sqlx::query("INSERT INTO shards (shard_id, name, dsn, accepts_new) VALUES ($1, $2, $3, false)")
        .bind(id.0)
        .bind(&marker)
        .bind(&url)
        .execute(meta.pool())
        .await
        .expect("shard");
    set_shard(&meta, scope, id).await;

    assert_eq!(resolver.shard_of(scope).await.expect("shard_of"), id);
    assert_eq!(count_mailboxes(resolver.as_ref(), scope).await, before);
    let pool = resolver.pool_for(scope).await.expect("pool_for");
    let app: String = sqlx::query_scalar("SELECT current_setting('application_name')")
        .fetch_one(&pool)
        .await
        .expect("fetch_one");
    assert_eq!(app, marker, "запрос ушёл в пул нового шарда");
    assert_eq!(
        resolver.place(scope.tenant).await.expect("place"),
        ShardId::LOCAL
    );

    set_shard(&meta, scope, ShardId::LOCAL).await;
    sqlx::query("DELETE FROM shards WHERE shard_id = $1")
        .bind(id.0)
        .execute(meta.pool())
        .await
        .expect("cleanup");
}
