//! META-70: журнал аудита Admin API и уровни политик на PostgreSQL — общие наборы CTR-25
//! `tp_testkit::suite::{admin_audit_log, policy_store}`.

mod common;

use tp_contracts::meta::{AccountStore, MailboxStore, SpecialUse};
use tp_testkit::suite::{self, PolicyFixture};

#[tokio::test]
async fn audit_log_passes_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let foreign = common::tenant(meta.pool()).await;
    suite::admin_audit_log(&meta, tenant, foreign).await;
}

#[tokio::test]
async fn policy_store_passes_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let domain = AccountStore::get(&meta, scope.tenant, scope.account)
        .await
        .expect("get")
        .expect("account")
        .domain;
    let mailbox = MailboxStore::list(&meta, scope)
        .await
        .expect("list")
        .into_iter()
        .find(|m| m.role == Some(SpecialUse::Inbox))
        .expect("inbox")
        .id;
    let fx = PolicyFixture {
        tenant: scope.tenant,
        foreign_tenant: common::tenant(meta.pool()).await,
        domain,
        account: scope.account,
        mailbox,
    };
    suite::policy_store(&meta, &meta, &fx).await;
}
