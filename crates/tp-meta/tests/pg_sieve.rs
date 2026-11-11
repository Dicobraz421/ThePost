//! META-49, META-50: скрипты Sieve, автоответ и журнал автоответов на PostgreSQL — общие наборы
//! `tp_testkit::suite::sieve`, RLS, шард ящика.

mod common;

use std::sync::Arc;
use std::time::Duration;

use sqlx::{AssertSqlSafe, PgPool};
use tp_contracts::meta::{AccountScope, AccountStore, AccountUpsert};
use tp_contracts::sieve::{SieveStore, Vacation, VacationLog};
use tp_contracts::{DomainId, Error, TenantId};
use tp_meta::sieve::SieveConfig;
use tp_meta::{MetaConfig, PgMeta, schema};
use tp_testkit::MemEventBus;
use tp_testkit::suite::sieve::{LIMITS, SieveFixture};
use tp_testkit::suite::{self, RealTime};

fn config() -> MetaConfig {
    MetaConfig {
        sieve: SieveConfig::with_limits(LIMITS),
        ..MetaConfig::default()
    }
}

/// Учётная запись организации `tenant` с доменом `domain`.
async fn account_in(meta: &PgMeta, tenant: TenantId, domain: (DomainId, &str)) -> AccountScope {
    let acc = meta
        .upsert(
            tenant,
            AccountUpsert {
                domain: domain.0,
                login: format!("u{}@{}", uuid::Uuid::now_v7().simple(), domain.1),
                display_name: "User".into(),
                external_id: None,
                enabled: true,
            },
        )
        .await
        .expect("upsert");
    AccountScope {
        tenant,
        account: acc.id,
    }
}

/// Две учётные записи одной организации.
async fn two_accounts(meta: &PgMeta) -> (AccountScope, AccountScope) {
    let tenant = common::tenant(meta.pool()).await;
    let (domain, name) = common::domain(meta.pool(), tenant).await;
    (
        account_in(meta, tenant, (domain, &name)).await,
        account_in(meta, tenant, (domain, &name)).await,
    )
}

#[tokio::test]
async fn passes_contract_suite() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let meta = PgMeta::new(pool.clone(), config());
    let (owner, peer) = two_accounts(&meta).await;
    let fx = SieveFixture {
        owner,
        peer,
        foreign: AccountScope {
            tenant: common::tenant(&pool).await,
            account: owner.account,
        },
    };
    suite::sieve(&meta, &fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signals_after_commit() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let bus = Arc::new(MemEventBus::default());
    let meta = PgMeta::new(pool, MetaConfig::default()).with_bus(bus.clone());
    let scope = common::account(&meta).await;
    suite::sieve::signals(&meta, bus.as_ref(), scope).await;
}

/// Чужая организация не видит скриптов, скрипта организации и автоответа (RLS, роль узлов `thepost_app`).
#[tokio::test]
async fn rls_hides_other_tenants() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let owner = common::account(&meta).await;
    meta.put(owner, "main", "keep;".into()).await.expect("put");
    meta.set_org_script(owner.tenant, Some("keep;".into()))
        .await
        .expect("org");
    meta.set_vacation(
        owner,
        Vacation {
            enabled: true,
            ..Vacation::default()
        },
    )
    .await
    .expect("vacation");
    let other = common::tenant(meta.pool()).await;
    let counts = async |tenant: TenantId| -> Vec<i64> {
        let mut tx = schema::begin(meta.pool(), tenant).await.expect("begin");
        sqlx::query("SET LOCAL ROLE thepost_app")
            .execute(&mut *tx)
            .await
            .expect("role");
        let mut out = Vec::new();
        for (table, column) in [
            ("sieve_scripts", "account_id"),
            ("vacation", "account_id"),
            ("sieve_org_scripts", "tenant_id"),
        ] {
            let id = if column == "account_id" {
                owner.account.as_uuid()
            } else {
                owner.tenant.as_uuid()
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
        let err = sqlx::query(
            "INSERT INTO sieve_scripts (tenant_id, account_id, name, script) VALUES ($1, $2, 'x', 'keep;')",
        )
        .bind(if tenant == owner.tenant {
            other.as_uuid()
        } else {
            owner.tenant.as_uuid()
        })
        .bind(owner.account.as_uuid())
        .execute(&mut *tx)
        .await
        .expect_err("чужая организация");
        assert!(matches!(schema::db_err(err), Error::Forbidden));
        tx.rollback().await.expect("rollback");
        out
    };
    assert_eq!(counts(owner.tenant).await, [1, 1, 1]);
    assert_eq!(counts(other).await, [0, 0, 0]);
}

/// Скрипты учётной записи второго шарда — в БД этого шарда; скрипт организации — в основной БД.
#[tokio::test]
async fn scripts_live_on_account_shard() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let tenant = t.tenant().await;
    let scope = t.account_on(tenant, t.shard).await;
    t.meta
        .put(scope, "main", "keep;".into())
        .await
        .expect("put");
    t.meta
        .activate(scope, Some("main"))
        .await
        .expect("activate");
    t.meta
        .set_org_script(scope.tenant, Some("stop;".into()))
        .await
        .expect("org");
    let count = async |pool: &PgPool, table: &str| -> i64 {
        let mut tx = schema::begin_system(pool).await.expect("begin");
        let n = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE tenant_id = $1"
        )))
        .bind(scope.tenant.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .expect("count");
        tx.rollback().await.expect("rollback");
        n
    };
    assert_eq!(count(&t.shard_pool, "sieve_scripts").await, 1);
    assert_eq!(count(t.main.meta.pool(), "sieve_scripts").await, 0);
    assert_eq!(count(t.main.meta.pool(), "sieve_org_scripts").await, 1);
    let active = t.meta.active(scope).await.expect("active").expect("main");
    assert_eq!(active.text, "keep;");
    assert_eq!(t.meta.state(scope).await.expect("state"), 2);
    t.drop_db().await;
}

/// META-50: набор `suite::sieve::vacation_log` — повтор в периоде не отвечает, после периода — отвечает.
#[tokio::test]
async fn vacation_log_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let (owner, peer) = two_accounts(&meta).await;
    suite::sieve::vacation_log(&meta, owner, peer, &RealTime).await;
}

/// Параллельные вызовы `seen` для одного отправителя: ответ получает один.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vacation_log_concurrent_single_reply() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let calls: Vec<_> = (0..16)
        .map(|_| {
            let meta = meta.clone();
            tokio::spawn(async move {
                meta.seen(scope, "boss@example.net", "", Duration::from_secs(3600))
                    .await
                    .expect("seen")
            })
        })
        .collect();
    let mut replies = 0;
    for c in calls {
        if !c.await.expect("join") {
            replies += 1;
        }
    }
    assert_eq!(replies, 1);
}

/// Очистка журнала старше срока: после неё отправителю снова отвечают. Своя БД: очистка — по всем организациям.
#[tokio::test]
async fn vacation_log_purge() {
    let Some(db) = common::fresh_meta().await else {
        return;
    };
    let meta = &db.meta;
    let scope = common::account(meta).await;
    let day = Duration::from_secs(24 * 3600);
    assert!(
        !meta
            .seen(scope, "a@example.net", "", day)
            .await
            .expect("seen")
    );
    assert!(
        meta.seen(scope, "a@example.net", "", day)
            .await
            .expect("seen")
    );
    assert_eq!(
        meta.purge_vacation_log().await.expect("purge"),
        0,
        "срок 90 суток"
    );
    let purged = meta
        .purge_vacation_log_before(time::OffsetDateTime::now_utc() + time::Duration::minutes(1))
        .await
        .expect("purge");
    assert_eq!(purged, 1);
    assert!(
        !meta
            .seen(scope, "a@example.net", "", day)
            .await
            .expect("seen"),
        "после очистки — снова ответ"
    );
    db.drop_db().await;
}
