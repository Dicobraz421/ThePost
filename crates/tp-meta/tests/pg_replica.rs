//! META-32: чтение с реплик для фоновых задач (`ReadIntent::Stale`).
//!
//! Реплики в CI нет: «реплика» — отдельная чистая БД со схемой (её сервер не в режиме восстановления —
//! отставание ноль). Чтение с неё видно по отсутствию данных основной БД.

mod common;

use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Executor, PgPool};
use time::OffsetDateTime;
use tp_contracts::meta::{
    AccountScope, AccountStore, MailboxStore, MessageStore, NewMessage, SpecialUse,
};
use tp_contracts::{BlobHash, BlobKey, Gen, MessageId};
use tp_meta::PgMeta;
use tp_meta::replica::Replica;
use tp_meta::schema;
use tp_meta::shards::{ReadIntent, ShardResolver};

/// Чистая БД со схемой: «реплика» без данных основной БД.
struct EmptyDb {
    pool: PgPool,
    admin: PgPool,
    name: String,
}

impl EmptyDb {
    async fn new(url: &str) -> Self {
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(url)
            .await
            .expect("connect");
        let name = format!("tp_replica_{}", uuid::Uuid::now_v7().simple());
        admin
            .execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .await
            .expect("create database");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(
                PgConnectOptions::from_str(url)
                    .expect("url")
                    .database(&name),
            )
            .await
            .expect("connect replica");
        schema::migrate(&pool).await.expect("schema");
        Self { pool, admin, name }
    }

    async fn drop_db(self) {
        self.pool.close().await;
        self.admin
            .execute(AssertSqlSafe(format!(
                "DROP DATABASE {} WITH (FORCE)",
                self.name
            )))
            .await
            .expect("drop");
    }
}

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
        size: 10,
        received_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts"),
        flags: Default::default(),
        meta: Default::default(),
    }
}

async fn delivered(meta: &PgMeta, scope: AccountScope) -> MessageId {
    let inbox = MailboxStore::list(meta, scope)
        .await
        .expect("list")
        .into_iter()
        .find(|m| m.role == Some(SpecialUse::Inbox))
        .expect("inbox")
        .id;
    meta.insert(scope, inbox, message(scope))
        .await
        .expect("insert")
        .id
}

async fn found(meta: &PgMeta, scope: AccountScope, id: MessageId) -> bool {
    MessageStore::get(meta, scope, id)
        .await
        .expect("get")
        .is_some()
}

#[tokio::test]
async fn stale_reads_replica_protocol_reads_primary() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let id = delivered(&meta, scope).await;
    let replica = EmptyDb::new(&url).await;
    let lag = Duration::from_secs(30);

    // Фоновая роль: чтения закрытого списка — на реплике (данных основной БД там нет), записи — на primary.
    let stale = meta
        .clone()
        .with_replica(replica.pool.clone(), lag)
        .with_intent(ReadIntent::Stale);
    assert!(!found(&stale, scope, id).await, "чтение с реплики");
    assert!(
        MailboxStore::list(&stale, scope)
            .await
            .expect("list")
            .is_empty(),
        "фоновое чтение папок не создаёт"
    );
    assert!(
        AccountStore::get(&stale, scope.tenant, scope.account)
            .await
            .expect("get")
            .is_none()
    );
    // Запись фоновой роли — на primary.
    let inbox = MailboxStore::list(&meta, scope)
        .await
        .expect("list")
        .into_iter()
        .find(|m| m.role == Some(SpecialUse::Inbox))
        .expect("inbox")
        .id;
    let written = stale
        .insert(scope, inbox, message(scope))
        .await
        .expect("insert");
    assert!(found(&meta, scope, written.id).await);

    // Протокольные хранилища (по умолчанию Primary) реплику не используют.
    let protocol = meta.clone().with_replica(replica.pool.clone(), lag);
    assert_eq!(protocol.intent(), ReadIntent::Primary);
    assert!(found(&protocol, scope, id).await);

    // Без реплики и при недоступной реплике — primary.
    let no_replica = meta.clone().with_intent(ReadIntent::Stale);
    assert!(found(&no_replica, scope, id).await);
    let dead = PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy("postgres://nobody@127.0.0.1:1/none")
        .expect("lazy");
    let unreachable = meta
        .clone()
        .with_replica(dead.clone(), lag)
        .with_intent(ReadIntent::Stale);
    assert!(found(&unreachable, scope, id).await);
    assert!(
        !Replica::new(dead, lag).usable().await,
        "недоступная реплика не годится"
    );

    replica.drop_db().await;
}

#[tokio::test]
async fn recovery_conflict_retries_on_primary() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let id = delivered(&meta, scope).await;
    // «Реплика», отменяющая чтение писем как при конфликте с восстановлением (40001).
    let replica = EmptyDb::new(&url).await;
    replica
        .pool
        .execute(
            "ALTER TABLE messages RENAME TO messages_real;
             CREATE FUNCTION tp_conflict() RETURNS SETOF messages_real LANGUAGE plpgsql AS $$
             BEGIN
                 RAISE EXCEPTION 'canceling statement due to conflict with recovery' USING ERRCODE = '40001';
             END $$;
             CREATE VIEW messages AS SELECT * FROM tp_conflict();",
        )
        .await
        .expect("conflict view");
    let stale = meta
        .clone()
        .with_replica(replica.pool.clone(), Duration::from_secs(30))
        .with_intent(ReadIntent::Stale);
    assert!(found(&stale, scope, id).await, "повтор на primary");

    replica.drop_db().await;
}

#[tokio::test]
async fn shard_replica_from_directory() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let Some(t) = common::two_shards().await else {
        return;
    };
    let org = t.tenant().await;
    let b = t.account_on(org, t.shard).await;
    let id = delivered(&t.meta, b).await;
    let replica = EmptyDb::new(&url).await;
    let (dsn, _) = common::split_password(&common::with_database(&url, &replica.name));
    sqlx::query("UPDATE shards SET read_replica = $2 WHERE shard_id = $1")
        .bind(t.shard.0)
        .bind(common::dev_url(&dsn))
        .execute(t.main.meta.pool())
        .await
        .expect("read_replica");
    t.dir.invalidate();

    let stale = t.meta.clone().with_intent(ReadIntent::Stale);
    assert!(!found(&stale, b, id).await, "чтение с реплики шарда");
    assert!(found(&t.meta, b, id).await, "протокольное — primary шарда");
    let listed = t.dir.list().await.expect("list");
    assert!(listed[1].read_replica.is_some());

    replica.drop_db().await;
    t.drop_db().await;
}
