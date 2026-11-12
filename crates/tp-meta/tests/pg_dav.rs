//! META-46…48: календари и контакты на PostgreSQL — общий набор `tp_testkit::suite::dav`, сигналы, RLS, миграция
//! на базе прежней схемы, шард ящика, очистка надгробий и устаревшие токены синхронизации.

mod common;

use std::str::FromStr;
use std::sync::Arc;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Executor, PgPool};
use time::OffsetDateTime;
use tp_contracts::dav::{
    Collection, CollectionKind, Component, DavStore, DeleteOutcome, ObjectIndex, Precondition,
    PutOutcome,
};
use tp_contracts::meta::{AccountScope, AccountStore, AccountUpsert};
use tp_contracts::{AccountId, DomainId, Error, ModSeq, TenantId};
use tp_meta::dav::DavConfig;
use tp_meta::pool::DbPool;
use tp_meta::{MetaConfig, PgMeta, schema};
use tp_testkit::MemEventBus;
use tp_testkit::suite;
use tp_testkit::suite::dav::{DavFixture, LIMITS};

fn config() -> MetaConfig {
    MetaConfig {
        dav: DavConfig::with_limits(LIMITS),
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

fn card(uid: &str) -> ObjectIndex {
    ObjectIndex::new(Component::Card, uid, None)
}

async fn put_card(meta: &PgMeta, scope: AccountScope, book: &Collection, name: &str) -> ModSeq {
    let out = meta
        .put_object(
            scope,
            book.id,
            name,
            "BEGIN:VCARD\r\nEND:VCARD\r\n".into(),
            card(name),
            Precondition::Any,
        )
        .await
        .expect("put");
    match out {
        PutOutcome::Stored(p) => p.modseq,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn passes_contract_suite() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let meta = PgMeta::new(pool.clone(), config());
    let tenant = common::tenant(&pool).await;
    let (domain, name) = common::domain(&pool, tenant).await;
    let owner = account_in(&meta, tenant, (domain, &name)).await;
    let peer = account_in(&meta, tenant, (domain, &name)).await;
    let fx = DavFixture {
        owner,
        peer,
        foreign: AccountScope {
            tenant: common::tenant(&pool).await,
            account: owner.account,
        },
    };
    suite::dav(&meta, &fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signals_after_commit() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let bus = Arc::new(MemEventBus::default());
    let meta = PgMeta::new(pool, MetaConfig::default()).with_bus(bus.clone());
    let scope = common::account(&meta).await;
    suite::dav::signals(&meta, bus.as_ref(), scope).await;
}

/// Чужая организация не видит строк календарей и контактов (RLS, роль узлов `thepost_app`).
#[tokio::test]
async fn rls_hides_other_tenants() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let owner = common::account(&meta).await;
    let book = Collection::new(CollectionKind::AddressBook, "contacts");
    meta.create_collection(owner, book.clone())
        .await
        .expect("create");
    put_card(&meta, owner, &book, "a.vcf").await;
    put_card(&meta, owner, &book, "b.vcf").await;
    meta.delete_object(owner, book.id, "b.vcf", Precondition::Any)
        .await
        .expect("delete");
    let other = common::tenant(meta.pool()).await;
    let counts = async |tenant: TenantId| -> Vec<i64> {
        let mut tx = schema::begin(meta.pool(), tenant).await.expect("begin");
        sqlx::query("SET LOCAL ROLE thepost_app")
            .execute(&mut *tx)
            .await
            .expect("role");
        let mut out = Vec::new();
        for table in ["dav_collections", "dav_objects", "dav_tombstones"] {
            let n: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT count(*) FROM {table} WHERE account_id = $1"
            )))
            .bind(owner.account.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .expect("count");
            out.push(n);
        }
        // Запись в чужую организацию запрещена политикой.
        let err = sqlx::query(
            "INSERT INTO dav_tombstones (tenant_id, account_id, modseq, kind, collection_id, name)
             VALUES ($1, $2, 1, 'calendar', $3, 'x')",
        )
        .bind(if tenant == owner.tenant {
            other.as_uuid()
        } else {
            owner.tenant.as_uuid()
        })
        .bind(owner.account.as_uuid())
        .bind(book.id.as_uuid())
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

/// Отдельная БД с миграциями до версии `version`; возвращает пул к ней и её имя.
async fn db_until(url: &str, version: i64) -> (PgPool, DbPool, String) {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
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
            PgConnectOptions::from_str(url)
                .expect("url")
                .database(&name),
        )
        .await
        .expect("connect db");
    schema::migrator_until(version)
        .run(&pool)
        .await
        .expect("migrate until");
    (admin, common::db(pool), name)
}

/// Миграция 0021 на базе прежней схемы: учётная запись, заведённая до миграции, получает календари.
#[tokio::test]
async fn migration_on_existing_db() {
    let Some(url) = common::pg_url() else { return };
    let (admin, pool, name) = db_until(&url, 20).await;
    let tenant = common::tenant(&pool).await;
    let (domain, _) = common::domain(&pool, tenant).await;
    let scope = AccountScope {
        tenant,
        account: AccountId::new(),
    };
    let mut tx = schema::begin(&pool, tenant).await.expect("begin");
    sqlx::query(
        "INSERT INTO accounts (tenant_id, account_id, domain_id, login) VALUES ($1, $2, $3, 'old')",
    )
    .bind(tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(domain.as_uuid())
    .execute(&mut *tx)
    .await
    .expect("account");
    sqlx::query("UPDATE mailbox_state SET modseq = 41 WHERE account_id = $1")
        .bind(scope.account.as_uuid())
        .execute(&mut *tx)
        .await
        .expect("modseq");
    tx.commit().await.expect("commit");

    schema::migrate(&pool).await.expect("migrate");
    let meta = PgMeta::new(pool.clone(), MetaConfig::default());
    assert_eq!(
        meta.state(scope, CollectionKind::Calendar)
            .await
            .expect("state"),
        ModSeq::ZERO
    );
    let m = meta
        .create_collection(scope, Collection::new(CollectionKind::Calendar, "work"))
        .await
        .expect("create");
    assert_eq!(m, ModSeq(42), "номер — из общего счётчика учётной записи");
    assert_eq!(
        meta.state(scope, CollectionKind::Calendar)
            .await
            .expect("state"),
        m
    );
    assert_eq!(
        meta.state(scope, CollectionKind::AddressBook)
            .await
            .expect("state"),
        ModSeq::ZERO
    );
    pool.close().await;
    admin
        .execute(AssertSqlSafe(format!("DROP DATABASE {name} WITH (FORCE)")))
        .await
        .expect("drop database");
}

/// Календари учётной записи второго шарда — в БД этого шарда, не в основной.
#[tokio::test]
async fn collections_live_on_account_shard() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let tenant = t.tenant().await;
    let scope = t.account_on(tenant, t.shard).await;
    let book = Collection::new(CollectionKind::AddressBook, "contacts");
    t.meta
        .create_collection(scope, book.clone())
        .await
        .expect("create");
    put_card(&t.meta, scope, &book, "a.vcf").await;
    let count = async |pool: &DbPool| -> i64 {
        let mut tx = schema::begin_system(pool).await.expect("begin");
        let n = sqlx::query_scalar("SELECT count(*) FROM dav_objects WHERE account_id = $1")
            .bind(scope.account.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .expect("count");
        tx.rollback().await.expect("rollback");
        n
    };
    assert_eq!(count(&t.shard_pool).await, 1);
    assert_eq!(count(t.main.meta.pool()).await, 0);
    let got = t
        .meta
        .get_object(scope, book.id, "a.vcf")
        .await
        .expect("get")
        .expect("a");
    assert_eq!(got.index, card("a.vcf"));
    t.drop_db().await;
}

/// META-48: очистка надгробий старше срока — токены до границы `Conflict`, полная синхронизация и токены после
/// границы работают; надгробия после очистки видны. Своя БД: очистка — по всем организациям.
#[tokio::test]
async fn purge_tombstones_invalidates_old_tokens() {
    let Some(db) = common::fresh_meta().await else {
        return;
    };
    let meta = &db.meta;
    let scope = common::account(meta).await;
    let book = Collection::new(CollectionKind::AddressBook, "contacts");
    let m0 = meta
        .create_collection(scope, book.clone())
        .await
        .expect("create");
    put_card(meta, scope, &book, "a.vcf").await;
    put_card(meta, scope, &book, "b.vcf").await;
    let m_c = put_card(meta, scope, &book, "c.vcf").await;
    let cal = Collection::new(CollectionKind::Calendar, "work");
    let m_cal = meta
        .create_collection(scope, cal.clone())
        .await
        .expect("calendar");
    let gone = Collection::new(CollectionKind::AddressBook, "old");
    meta.create_collection(scope, gone.clone())
        .await
        .expect("create");
    put_card(meta, scope, &gone, "x.vcf").await;
    let DeleteOutcome::Deleted(m_del) = meta
        .delete_object(scope, book.id, "a.vcf", Precondition::Any)
        .await
        .expect("delete")
    else {
        panic!("не удалён");
    };
    meta.delete_collection(scope, gone.id)
        .await
        .expect("delete collection");
    // По умолчанию срок — 90 суток: свежие надгробия остаются.
    assert_eq!(meta.purge_dav_tombstones().await.expect("purge"), 0);
    let before = meta
        .changes(scope, book.id, m0, 100)
        .await
        .expect("changes");
    assert_eq!(before.destroyed, ["a.vcf"]);

    let purged = meta
        .purge_dav_tombstones_before(OffsetDateTime::now_utc() + time::Duration::minutes(1))
        .await
        .expect("purge");
    assert_eq!(purged, 3, "объект, объект удалённой коллекции, коллекция");
    let floor = meta
        .state(scope, CollectionKind::AddressBook)
        .await
        .expect("state");
    assert_eq!(
        meta.changes(scope, book.id, m0, 100).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        meta.changes(scope, book.id, m_c, 100).await,
        Err(Error::Conflict),
        "токен до очистки"
    );
    // Граница коллекции — её последнее очищенное надгробие: токен на ней действует.
    let at_coll_floor = meta
        .changes(scope, book.id, m_del, 100)
        .await
        .expect("на границе коллекции");
    assert!(at_coll_floor.destroyed.is_empty() && at_coll_floor.created.is_empty());
    // Очистка адресных книг не трогает токены календарей.
    let cc = meta
        .collection_changes(scope, CollectionKind::Calendar, m_cal, 100)
        .await
        .expect("календари");
    assert_eq!(cc.new_state, m_cal);
    assert_eq!(
        meta.account_changes(scope, CollectionKind::AddressBook, m0, 100)
            .await,
        Err(Error::Conflict)
    );
    assert_eq!(
        meta.collection_changes(scope, CollectionKind::AddressBook, m0, 100)
            .await,
        Err(Error::Conflict)
    );
    // Полная синхронизация и токен на границе.
    let full = meta
        .changes(scope, book.id, ModSeq::ZERO, 100)
        .await
        .expect("full");
    let mut names = full.created.clone();
    names.sort();
    assert_eq!(names, ["b.vcf", "c.vcf"]);
    assert!(full.destroyed.is_empty());
    assert_eq!(
        full.new_state, m_del,
        "токен полной синхронизации — состояние коллекции"
    );
    let at_floor = meta
        .account_changes(scope, CollectionKind::AddressBook, floor, 100)
        .await
        .expect("на границе");
    assert!(at_floor.created.is_empty() && at_floor.destroyed.is_empty());
    assert_eq!(at_floor.new_state, floor);
    // Новое удаление после очистки видно с токена на границе.
    let DeleteOutcome::Deleted(m_b) = meta
        .delete_object(scope, book.id, "b.vcf", Precondition::Any)
        .await
        .expect("delete")
    else {
        panic!("не удалён");
    };
    let ch = meta
        .changes(scope, book.id, full.new_state, 100)
        .await
        .expect("changes");
    assert_eq!(ch.destroyed, ["b.vcf"]);
    assert_eq!(ch.new_state, m_b);
    // Состояние вида не уменьшается после очистки.
    assert_eq!(
        meta.state(scope, CollectionKind::AddressBook)
            .await
            .expect("state"),
        m_b
    );
    db.drop_db().await;
}
