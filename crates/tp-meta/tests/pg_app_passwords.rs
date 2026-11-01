//! CTR-21: пароли приложений на PostgreSQL — общий набор `tp_testkit::suite::app_password_store`.

mod common;

use tp_contracts::meta::{AccountScope, AccountStore, AccountUpsert};
use tp_testkit::suite::{AppPasswordFixture, app_password_store};

#[tokio::test]
async fn passes_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let owner = common::account(&meta).await;
    let (domain, name) = common::domain(meta.pool(), owner.tenant).await;
    let peer = meta
        .upsert(
            owner.tenant,
            AccountUpsert {
                domain,
                login: format!("peer@{name}"),
                display_name: "Peer".into(),
                external_id: None,
                enabled: true,
            },
        )
        .await
        .expect("peer");
    let fx = AppPasswordFixture {
        owner,
        peer: AccountScope {
            tenant: owner.tenant,
            account: peer.id,
        },
        foreign_tenant: common::tenant(meta.pool()).await,
    };
    app_password_store(&meta, &fx).await;
}
