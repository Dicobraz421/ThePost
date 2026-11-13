//! CTR-73, CTR-76…78 на PostgreSQL: ближайший срок и сигнал очереди, архив ящика (и на втором шарде), сигнал и
//! отметка удаления отключённого ящика — общие наборы `tp_testkit::suite::lifecycle`.

mod common;

use std::sync::Arc;

use tp_testkit::MemEventBus;
use tp_testkit::suite::AccountStateFixture;
use tp_testkit::suite::lifecycle::{self, ArchiveStores};

/// Очередь — на своей чистой БД: `next_due` видит записи всех организаций.
#[tokio::test]
async fn queue_next_due_suite() {
    let Some(db) = common::fresh_meta().await else {
        return;
    };
    let bus = Arc::new(MemEventBus::default());
    let meta = db.meta.clone().with_bus(bus.clone());
    let tenant = common::tenant(meta.pool()).await;
    lifecycle::queue_next_due(&meta, bus.as_ref(), tenant).await;
    db.drop_db().await;
}

#[tokio::test]
async fn disabled_mailbox_suites() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let bus = Arc::new(MemEventBus::default());
    let meta = meta.with_bus(bus.clone());
    let tenant = common::tenant(meta.pool()).await;
    let (domain, domain_name) = common::domain(meta.pool(), tenant).await;
    let fx = AccountStateFixture {
        tenant,
        domain,
        domain_name,
    };
    lifecycle::disabled_mailbox_signals(&meta, bus.as_ref(), &fx).await;
    lifecycle::account_removal(&meta, &fx).await;
}

#[tokio::test]
async fn archive_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let (domain, domain_name) = common::domain(meta.pool(), tenant).await;
    let fx = AccountStateFixture {
        tenant,
        domain,
        domain_name,
    };
    let stores = ArchiveStores {
        archive: &meta,
        quota: &meta,
        accounts: &meta,
        mailboxes: &meta,
        messages: &meta,
        policies: &meta,
    };
    lifecycle::archive(&stores, &fx).await;
}

/// Ящики организации — на втором шарде: письма и папки там, политика и учётная запись — в основной БД.
#[tokio::test]
async fn archive_suite_on_second_shard() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let (tenant, domain, domain_name) = t.tenant().await;
    t.account_on((tenant, domain, domain_name.clone()), t.shard)
        .await;
    let fx = AccountStateFixture {
        tenant,
        domain,
        domain_name,
    };
    let stores = ArchiveStores {
        archive: &t.meta,
        quota: &t.meta,
        accounts: &t.meta,
        mailboxes: &t.meta,
        messages: &t.meta,
        policies: &t.meta,
    };
    lifecycle::archive(&stores, &fx).await;
    t.drop_db().await;
}
