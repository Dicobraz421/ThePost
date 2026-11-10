//! Блокировки источников и учётных записей на PostgreSQL (META-27, CONTRACTS.md#AbuseBlocks); миграция записей
//! узлов (адрес, площадка).

mod common;

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Executor};
use time::OffsetDateTime;
use tp_contracts::abuse::{AbuseBlocks, BlockSubject};
use tp_contracts::bus::{Event, EventKind};
use tp_contracts::membership::{Membership, NodeId, SiteId};
use tp_contracts::{AccountId, Error, TenantId};
use tp_meta::{MetaConfig, PgMeta, schema};
use tp_testkit::suite::{self, RealTime};

async fn db_now(meta: &PgMeta) -> OffsetDateTime {
    sqlx::query_scalar("SELECT now()")
        .fetch_one(meta.pool())
        .await
        .expect("now")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_abuse_blocks() {
    let Some(meta) = common::meta().await else {
        return;
    };
    // Набор усекает сроки до секунды (до 1 с раньше `now`), поэтому `ttl / 2` должен быть с запасом больше секунды.
    let now = db_now(&meta).await;
    suite::abuse_blocks(&meta, now, Duration::from_secs(4), &RealTime).await;
}

#[tokio::test]
async fn blocked_signal_and_isolation() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let rec = Arc::new(common::RecordingBus::default());
    let meta = PgMeta::new(pool, MetaConfig::default()).with_bus(rec.clone());
    let tenant = TenantId::new();
    let now = db_now(&meta).await;
    let until = now + Duration::from_secs(600);
    let src = BlockSubject::Ip("2001:db8::7".parse().expect("ip"));
    meta.block(tenant, &src, until, "перебор паролей")
        .await
        .expect("block");
    assert_eq!(
        rec.take(),
        vec![Event {
            tenant,
            account: None,
            kind: EventKind::Blocked { subject: src },
        }]
    );
    // Срок в прошлом — Invalid, без сигнала.
    assert!(matches!(
        meta.block(tenant, &src, now - Duration::from_secs(1), "past")
            .await,
        Err(Error::Invalid(_))
    ));
    assert!(rec.take().is_empty());
    // Подсеть IPv6 покрывает адрес; IPv4 той же записи — нет; адрес /128 не покрывает подсеть /128.
    let net = BlockSubject::net("2001:db8::".parse().expect("ip"), 64).expect("net");
    meta.block(tenant, &net, until, "net").await.expect("net");
    let inner = BlockSubject::Ip("2001:db8::1:2".parse().expect("ip"));
    assert!(
        meta.blocked(tenant, &inner)
            .await
            .expect("blocked")
            .is_some()
    );
    let v4 = BlockSubject::Ip("32.1.13.184".parse().expect("ip"));
    assert_eq!(meta.blocked(tenant, &v4).await, Ok(None));
    meta.unblock(tenant, &net).await.expect("unblock");
    let net128 = BlockSubject::net("2001:db8::7".parse().expect("ip"), 128).expect("net");
    assert_eq!(meta.blocked(tenant, &net128).await, Ok(None));

    // RLS включена и обязательна для владельца таблицы (роль теста может её обходить как суперпользователь).
    let (rls, forced): (bool, bool) = sqlx::query_as(
        "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE relname = 'abuse_blocks'",
    )
    .fetch_one(meta.pool())
    .await
    .expect("pg_class");
    assert!(rls && forced);
    let other = TenantId::new();
    assert_eq!(meta.active(other).await, Ok(Vec::new()));
    let account = BlockSubject::Account(AccountId::new());
    assert_eq!(meta.blocked(other, &account).await, Ok(None));
}

#[tokio::test]
async fn expired_rows_are_purged() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let meta = PgMeta::new(pool, MetaConfig::default());
    let tenant = TenantId::new();
    // Срок — с запасом на нагрузку CI: пять записей должны успеть до него (было 300 мс — «срок в прошлом»).
    let short = Duration::from_secs(2);
    let until = db_now(&meta).await + short;
    for i in 0..5u8 {
        let s = BlockSubject::Ip(format!("192.0.2.{i}").parse().expect("ip"));
        meta.block(tenant, &s, until, "short").await.expect("block");
    }
    while db_now(&meta).await <= until + Duration::from_millis(100) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let s = BlockSubject::Ip("192.0.2.100".parse().expect("ip"));
    meta.block(
        tenant,
        &s,
        db_now(&meta).await + Duration::from_secs(60),
        "long",
    )
    .await
    .expect("block");
    let rows: i64 = tp_meta::pool::with_tenant(meta.pool(), tenant, |tx| {
        Box::pin(async move {
            sqlx::query_scalar("SELECT count(*) FROM abuse_blocks WHERE tenant_id = $1")
                .bind(tenant.as_uuid())
                .fetch_one(&mut *tx)
                .await
                .map_err(schema::db_err)
        })
    })
    .await
    .expect("count");
    assert_eq!(rows, 1, "истёкшие строки удалены");
    assert_eq!(meta.active(tenant).await.expect("active").len(), 1);
}

#[tokio::test]
async fn migration_keeps_node_records() {
    let Some(url) = common::pg_url() else { return };
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect");
    let db = format!("tp_meta_{}", uuid::Uuid::now_v7().simple());
    admin
        .execute(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .await
        .expect("create database");
    let opts = PgConnectOptions::from_str(&url).expect("url").database(&db);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .expect("connect clean");

    // Схема до META-27 с записью узла.
    schema::migrator_until(14)
        .run(&pool)
        .await
        .expect("old schema");
    sqlx::query(
        "INSERT INTO nodes (node_id, roles, expires_at) VALUES ('old-node', '{imap}', now() + interval '1 hour')",
    )
    .execute(&pool)
    .await
    .expect("old row");
    schema::migrate(&pool).await.expect("migrate");

    let meta = PgMeta::new(pool.clone(), MetaConfig::default());
    let live = meta.live_info().await.expect("live_info");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, NodeId::new("old-node").expect("id"));
    assert_eq!(live[0].addr, "");
    assert_eq!(live[0].site, SiteId::default());
    assert_eq!(live[0].roles.len(), 1);
    let now = db_now(&meta).await;
    meta.block(
        TenantId::DEFAULT,
        &BlockSubject::Ip("198.51.100.1".parse().expect("ip")),
        now + Duration::from_secs(60),
        "after migration",
    )
    .await
    .expect("block");

    pool.close().await;
    admin
        .execute(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .await
        .expect("drop database");
}
