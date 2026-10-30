//! Справочник шардов (META-08); учётные данные шардов из хранилища секретов (META-17).

mod common;

use std::sync::Arc;

use tp_contracts::meta::AccountScope;
use tp_contracts::secrets::SecretRef;
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

fn new_shard_id() -> ShardId {
    ShardId(i32::try_from(Uuid::now_v7().as_u128() % 1_000_000_000).expect("as_u128") + 1)
}

async fn insert_shard(
    meta: &tp_meta::PgMeta,
    id: ShardId,
    name: &str,
    dsn: &str,
    credentials: Option<&SecretRef>,
) -> tp_contracts::Result<()> {
    sqlx::query(
        "INSERT INTO shards (shard_id, name, dsn, credentials, accepts_new) VALUES ($1, $2, $3, $4, false)",
    )
    .bind(id.0)
    .bind(name)
    .bind(dsn)
    .bind(credentials.map(ToString::to_string))
    .execute(meta.pool())
    .await
    .map_err(db_err)?;
    Ok(())
}

async fn delete_shard(meta: &tp_meta::PgMeta, id: ShardId) {
    sqlx::query("DELETE FROM shards WHERE shard_id = $1")
        .bind(id.0)
        .execute(meta.pool())
        .await
        .expect("cleanup");
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
    let dir = ShardDirectory::new(
        meta.pool().clone(),
        PoolConfig::default(),
        common::secrets(),
    );
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
            ..common::dev_pool_config(&url)
        },
        common::secrets(),
    ));
    let before = count_mailboxes(resolver.as_ref(), scope).await;

    // Второй шард — та же БД под другим адресом; учётная запись переключается в справочнике.
    let id = new_shard_id();
    let (dsn, password) = common::split_password(&url);
    let credentials = password.map(|p| {
        let r = common::db_secret();
        common::secrets().set(&r, &p);
        r
    });
    insert_shard(
        &meta,
        id,
        &marker,
        &common::dev_url(&dsn),
        credentials.as_ref(),
    )
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

#[tokio::test]
async fn shard_password_from_secret_store() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let Some(meta) = common::meta().await else {
        return;
    };
    let (dsn, password) = common::split_password(&url);
    let dsn = common::dev_url(&dsn);
    let secret: SecretRef = format!("test/shard-{}#password", Uuid::now_v7().simple())
        .parse()
        .expect("ссылка");
    let store = common::secrets();
    store.set(&secret, password.as_deref().unwrap_or("unused"));
    let marker = format!("shard-{}", Uuid::now_v7().simple());
    let dir = ShardDirectory::new(
        meta.pool().clone(),
        PoolConfig {
            application_name: marker.clone(),
            ..common::dev_pool_config(&url)
        },
        store.clone(),
    );
    let id = new_shard_id();
    insert_shard(&meta, id, &marker, &dsn, Some(&secret))
        .await
        .expect("shard");

    // Пул шарда подключается паролем из хранилища; справочник показывает ссылку, не значение.
    let pool = dir.pool(id).await.expect("pool");
    let app: String = sqlx::query_scalar("SELECT current_setting('application_name')")
        .fetch_one(&pool)
        .await
        .expect("fetch_one");
    assert_eq!(app, marker);
    let listed = dir.list().await.expect("list");
    let row = listed.iter().find(|s| s.id == id).expect("row");
    assert_eq!(row.credentials.as_ref(), Some(&secret));
    if let Some(p) = &password {
        assert!(!format!("{row:?}").contains(p.as_str()));
    }

    // Отзыв секрета: кэшированный пул живёт до forget, затем — понятная ошибка Unavailable.
    store.revoke(&secret);
    assert!(dir.pool(id).await.is_ok(), "пул из кэша");
    dir.forget(id);
    assert_eq!(dir.pool(id).await.err(), Some(Error::UNAVAILABLE));
    // Новый секрет — пул пересоздаётся.
    store.set(&secret, password.as_deref().unwrap_or("unused"));
    let pool = dir.pool(id).await.expect("pool после смены секрета");
    sqlx::query("SELECT 1")
        .execute(&pool)
        .await
        .expect("select");

    delete_shard(&meta, id).await;
}

#[tokio::test]
async fn shard_dsn_with_password_rejected() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let id = new_shard_id();
    // Ограничение таблицы: пароль в адресе и неверная ссылка на секрет.
    for dsn in [
        "postgres://u:hunter2@db/shard",
        "postgres://u@db/shard?password=hunter2",
    ] {
        assert!(
            matches!(
                insert_shard(&meta, id, "with-password", dsn, None).await,
                Err(Error::Invalid(_))
            ),
            "{dsn}"
        );
    }
    let bad_ref = sqlx::query(
        "INSERT INTO shards (shard_id, name, dsn, credentials) VALUES ($1, 'bad-ref', 'postgres://db/s', 'hunter2')",
    )
    .bind(id.0)
    .execute(meta.pool())
    .await
    .map_err(db_err);
    assert!(matches!(bad_ref, Err(Error::Invalid(_))), "{bad_ref:?}");
}
