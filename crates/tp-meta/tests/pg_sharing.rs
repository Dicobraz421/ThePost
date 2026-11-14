//! META-52: права на коллекции, представители, правила бронирования ресурсов и политика общего доступа на PostgreSQL —
//! общие наборы `tp_testkit::suite::sharing` (CTR-48) и RLS. META-83: уведомления о доступе (CTR-86) — набор
//! `share_notifications` и миграция 0042.

mod common;

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Executor};
use time::OffsetDateTime;
use tp_contracts::dav::{Collection, CollectionId, CollectionKind, DavStore};
use tp_contracts::meta::{AccountKind, AccountScope, AccountStore, AccountUpsert};
use tp_contracts::sharing::{
    CollectionAcl, CollectionRights, Delegate, DelegateLevel, DelegateStore, ShareChangedBy,
    ShareNotification, ShareNotificationStore, SharedObjectType, SharingPolicy, SharingPolicyStore,
    SharingScope,
};
use tp_contracts::{AccountId, ShareNotificationId, TenantId};
use tp_meta::{MetaConfig, PgMeta, schema};
use tp_testkit::suite::sharing::{
    SharingFixture, collection_acl, delegate_store, resource_store, share_notifications,
    sharing_policy_store,
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

/// CTR-86 на своей чистой БД: срок хранения удаляет уведомления всех организаций. Получатели — разных организаций,
/// затем одной.
#[tokio::test]
async fn share_notifications_pass_contract_suite() {
    let Some(db) = common::fresh_meta().await else {
        return;
    };
    let meta = &db.meta;
    let recipient = common::account(meta).await;
    share_notifications(meta, recipient, common::account(meta).await).await;
    let recipient = common::account(meta).await;
    let peer = account_like(meta, recipient, AccountKind::User).await;
    share_notifications(meta, recipient, peer).await;
    db.drop_db().await;
}

/// Миграция 0042 поверх 0041 на базе с учётной записью: повторный запуск ничего не меняет; уведомления
/// записываются; удаление учётной записи-получателя удаляет уведомления, состояние и журнал.
#[tokio::test]
async fn share_notifications_migration_on_existing_db() {
    let Some(url) = common::pg_url() else { return };
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect");
    let name = format!("tp_meta_{}", uuid::Uuid::now_v7().simple());
    admin
        .execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .await
        .expect("create database");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(
            PgConnectOptions::from_str(&url)
                .expect("url")
                .database(&name),
        )
        .await
        .expect("connect db");
    schema::migrator_until(41)
        .run(&pool)
        .await
        .expect("migrate until");
    let pool = common::db(pool);
    let tenant = common::tenant(&pool).await;
    let (domain, _) = common::domain(&pool, tenant).await;
    let account = AccountId::new();
    let mut tx = schema::begin(&pool, tenant).await.expect("begin");
    sqlx::query(
        "INSERT INTO accounts (tenant_id, account_id, domain_id, login) VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant.as_uuid())
    .bind(account.as_uuid())
    .bind(domain.as_uuid())
    .bind("user@example.org")
    .execute(&mut *tx)
    .await
    .expect("account");
    tx.commit().await.expect("commit");

    schema::migrate(&pool).await.expect("migrate");
    let tables = async || -> i64 {
        let mut tx = schema::begin(&pool, tenant).await.expect("begin");
        let n = sqlx::query_scalar(
            "SELECT count(*) FROM pg_tables WHERE schemaname = 'public' AND tablename LIKE 'share_notification%'",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("tables");
        tx.commit().await.expect("commit");
        n
    };
    assert_eq!(tables().await, 3);
    schema::migrate(&pool).await.expect("повторный запуск");
    assert_eq!(tables().await, 3);

    let meta = PgMeta::new(pool.clone(), MetaConfig::default());
    let scope = AccountScope { tenant, account };
    let notice = ShareNotification {
        id: ShareNotificationId::new(),
        created: OffsetDateTime::UNIX_EPOCH,
        changed_by: ShareChangedBy {
            name: "Админ".into(),
            email: None,
            account: None,
        },
        object_type: SharedObjectType::AddressBook,
        owner: AccountId::new(),
        collection: CollectionId::new(),
        old_rights: CollectionRights::WRITE,
        new_rights: CollectionRights::NONE,
        name: "Контакты".into(),
    };
    assert_eq!(meta.record(scope, notice.clone()).await, Ok(1));
    assert_eq!(meta.notifications(scope).await, Ok(vec![notice]));

    let mut tx = schema::begin(&pool, tenant).await.expect("begin");
    sqlx::query("DELETE FROM accounts WHERE tenant_id = $1 AND account_id = $2")
        .bind(tenant.as_uuid())
        .bind(account.as_uuid())
        .execute(&mut *tx)
        .await
        .expect("delete account");
    let left: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM share_notifications) + (SELECT count(*) FROM share_notification_state)
              + (SELECT count(*) FROM share_notification_log)",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("count");
    tx.commit().await.expect("commit");
    assert_eq!(left, 0, "удаление учётной записи удаляет уведомления");
    assert_eq!(meta.notification_state(scope).await, Ok(0));

    pool.close().await;
    admin
        .execute(AssertSqlSafe(format!("DROP DATABASE {name} WITH (FORCE)")))
        .await
        .expect("drop database");
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
