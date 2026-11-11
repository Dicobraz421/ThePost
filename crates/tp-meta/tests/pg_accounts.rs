//! CTR-44: постраничный список учётных записей на PostgreSQL — общий набор `tp_testkit::suite::account_list`
//! (страницы, курсор, префикс, регистр, изоляция организаций).

mod common;

use tp_testkit::suite::{AccountListFixture, account_list};

#[tokio::test]
async fn passes_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let (domain, domain_name) = common::domain(meta.pool(), tenant).await;
    let foreign_tenant = common::tenant(meta.pool()).await;
    let (foreign_domain, _) = common::domain(meta.pool(), foreign_tenant).await;
    let fx = AccountListFixture {
        tenant,
        domain,
        domain_name,
        foreign_tenant,
        foreign_domain,
    };
    account_list(&meta, &fx).await;
}
