//! META-33: списки отправителей, контакты, оргструктура, динамические группы, состояние учётной записи и отмена
//! отправки на PostgreSQL — общие наборы `tp_testkit::suite`; состояние ящика — и на втором шарде.

mod common;

use tp_contracts::groups::{DistributionGroup, GroupStore, SenderRestriction};
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, GroupId, TenantId};
use tp_meta::PgMeta;
use tp_testkit::suite::{self, AccountStateFixture, DynamicFixture};

#[tokio::test]
async fn sender_lists_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let scope = |tenant| AccountScope {
        tenant,
        account: AccountId::new(),
    };
    suite::sender_lists(&meta, scope(tenant), scope(tenant)).await;
}

#[tokio::test]
async fn contact_store_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let foreign = common::tenant(meta.pool()).await;
    suite::contact_store(&meta, tenant, foreign).await;
}

async fn accounts(meta: &PgMeta, tenant: TenantId, n: usize) -> Vec<AccountId> {
    let (domain, name) = common::domain(meta.pool(), tenant).await;
    let mut out = Vec::new();
    for _ in 0..n {
        let acc = tp_contracts::meta::AccountStore::upsert(
            meta,
            tenant,
            tp_contracts::meta::AccountUpsert {
                domain,
                login: format!("u{}@{name}", uuid::Uuid::now_v7().simple()),
                display_name: "User".into(),
                external_id: None,
                enabled: true,
                aliases: Vec::new(),
                kind: tp_contracts::meta::AccountKind::User,
            },
        )
        .await
        .expect("upsert");
        out.push(acc.id);
    }
    out
}

#[tokio::test]
async fn org_store_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let accounts = accounts(&meta, tenant, 3).await;
    suite::org_store(&meta, tenant, &accounts).await;
}

#[tokio::test]
async fn dynamic_groups_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let group = DistributionGroup {
        id: GroupId::new(),
        tenant,
        name: "dynamic".into(),
        address: None,
        external_id: None,
        members: Vec::new(),
        senders: SenderRestriction::Anyone,
        moderators: Vec::new(),
        hidden_from_gal: false,
    };
    GroupStore::put(&meta, group.clone()).await.expect("группа");
    let fx = DynamicFixture {
        tenant,
        group: group.id,
        accounts: accounts(&meta, tenant, 3).await,
    };
    suite::dynamic_groups(&meta, &meta, &meta, &fx).await;
}

#[tokio::test]
async fn account_state_suite() {
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
    suite::account_state(&meta, &meta, &meta, &fx).await;
}

/// Набор на втором шарде: все учётные записи организации размещаются на нём.
#[tokio::test]
async fn account_state_suite_on_second_shard() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let (tenant, domain, domain_name) = t.tenant().await;
    // Размещение новых учётных записей — на втором шарде.
    t.account_on((tenant, domain, domain_name.clone()), t.shard)
        .await;
    let fx = AccountStateFixture {
        tenant,
        domain,
        domain_name,
    };
    suite::account_state(&t.meta, &t.meta, &t.meta, &fx).await;
    t.drop_db().await;
}

#[tokio::test]
async fn queue_release_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    suite::queue_release(&meta, tenant).await;
}
