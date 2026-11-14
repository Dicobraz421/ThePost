//! META-52: права на коллекции, представители, правила бронирования ресурсов и политика общего доступа на PostgreSQL —
//! общие наборы `tp_testkit::suite::sharing` (CTR-48) и RLS.

mod common;

use sqlx::AssertSqlSafe;
use tp_contracts::TenantId;
use tp_contracts::dav::{Collection, CollectionKind, DavStore};
use tp_contracts::meta::{AccountKind, AccountScope, AccountStore, AccountUpsert};
use tp_contracts::sharing::{
    CollectionAcl, CollectionRights, Delegate, DelegateLevel, DelegateStore, SharingPolicy,
    SharingPolicyStore, SharingScope,
};
use tp_meta::{PgMeta, schema};
use tp_testkit::suite::sharing::{
    SharingFixture, collection_acl, delegate_store, resource_store, sharing_policy_store,
};

/// Учётная запись вида `kind` организации `owner`.
async fn account_like(meta: &PgMeta, owner: AccountScope, kind: AccountKind) -> AccountScope {
    let (domain, name) = common::domain(meta.pool(), owner.tenant).await;
    let acc = meta
        .upsert(
            owner.tenant,
            AccountUpsert {
                domain,
                login: format!("u{}@{name}", uuid::Uuid::now_v7().simple()),
                display_name: "User".into(),
                external_id: None,
                enabled: true,
                aliases: Vec::new(),
                kind,
                hidden_from_gal: false,
                resource_type: None,
            },
        )
        .await
        .expect("upsert");
    AccountScope {
        tenant: owner.tenant,
        account: acc.id,
    }
}

async fn fixture(meta: &PgMeta) -> SharingFixture {
    let owner = common::account(meta).await;
    SharingFixture {
        owner,
        peer: account_like(meta, owner, AccountKind::User).await,
        resource: account_like(meta, owner, AccountKind::Resource).await,
        foreign: common::account(meta).await,
    }
}

#[tokio::test]
async fn collection_acl_passes_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let fx = fixture(&meta).await;
    collection_acl(&meta, &meta, &fx).await;
}

#[tokio::test]
async fn delegates_pass_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let fx = fixture(&meta).await;
    delegate_store(&meta, &fx).await;
}

#[tokio::test]
async fn resources_pass_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let fx = fixture(&meta).await;
    resource_store(&meta, &meta, &fx).await;
}

#[tokio::test]
async fn sharing_policy_passes_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let fx = fixture(&meta).await;
    sharing_policy_store(&meta, fx.owner.tenant, fx.foreign.tenant).await;
}

/// Чужая организация не видит строк общего доступа (RLS, роль узлов `thepost_app`).
#[tokio::test]
async fn rls_hides_other_tenants() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let fx = fixture(&meta).await;
    let cal = Collection::new(CollectionKind::Calendar, "shared");
    meta.create_collection(fx.owner, cal.clone())
        .await
        .expect("create");
    meta.set_collection_acl(fx.owner, cal.id, "bob", CollectionRights::READ)
        .await
        .expect("acl");
    meta.set_delegate(
        fx.owner,
        Delegate {
            delegate: fx.peer.account,
            level: DelegateLevel::Read,
            schedule_on_behalf: false,
        },
    )
    .await
    .expect("delegate");
    meta.set_sharing_policy(
        fx.owner.tenant,
        SharingPolicy {
            scope: SharingScope::Nobody,
            ..SharingPolicy::default()
        },
    )
    .await
    .expect("policy");
    let counts = async |tenant: TenantId| -> Vec<i64> {
        let mut tx = schema::begin(meta.pool(), tenant).await.expect("begin");
        sqlx::query("SET LOCAL ROLE thepost_app")
            .execute(&mut *tx)
            .await
            .expect("role");
        let mut out = Vec::new();
        for (table, column) in [
            ("collection_acl", "account_id"),
            ("account_delegates", "account_id"),
            ("sharing_policies", "tenant_id"),
        ] {
            let id = if column == "tenant_id" {
                fx.owner.tenant.as_uuid()
            } else {
                fx.owner.account.as_uuid()
            };
            let n: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT count(*) FROM {table} WHERE {column} = $1"
            )))
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .expect("count");
            out.push(n);
        }
        tx.rollback().await.expect("rollback");
        out
    };
    assert_eq!(counts(fx.owner.tenant).await, [1, 1, 1]);
    assert_eq!(counts(fx.foreign.tenant).await, [0, 0, 0]);
}
