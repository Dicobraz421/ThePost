//! META-51: планирование встреч на PostgreSQL — наборы `tp_testkit::suite::freebusy` и `suite::itip`, сигналы
//! входящих, RLS новых таблиц, миграция 0024 на базе прежней схемы, занятость учётных записей разных шардов.
//! META-54: набор `suite::freebusy_recurring` (разворот повторов при запросе) и миграция 0025.

mod common;

use std::str::FromStr;
use std::sync::Arc;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Executor};
use time::{Duration, OffsetDateTime};
use tp_contracts::dav::{
    Collection, CollectionKind, Component, DavStore, ObjectIndex, Precondition, PutOutcome, Span,
};
use tp_contracts::meta::{AccountScope, AccountStore, AccountUpsert};
use tp_contracts::schedule::{BusyKind, BusyPeriod, FreeBusy, InboxMessage, ItipMethod, Period};
use tp_contracts::{AccountId, DomainId, Error, ModSeq, TenantId};
use tp_meta::dav::DavConfig;
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

/// Хранилище с лимитами набора и учётные записи набора без данных.
async fn fixture() -> Option<(PgMeta, DavFixture)> {
    let pool = common::pool().await?;
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
    Some((meta, fx))
}

fn at(h: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000) + Duration::hours(h)
}

fn busy(s: i64, e: i64) -> BusyPeriod {
    BusyPeriod {
        start: at(s),
        end: at(e),
        kind: BusyKind::Busy,
    }
}

fn message(uid: &str) -> InboxMessage {
    InboxMessage {
        method: ItipMethod::Request,
        uid: uid.into(),
        from: "boss@partner.example".into(),
        body: "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".into(),
    }
}

/// Событие `name` календаря `cal` с периодами занятости `periods`.
async fn put_event(
    meta: &PgMeta,
    scope: AccountScope,
    cal: &Collection,
    name: &str,
    periods: Vec<BusyPeriod>,
) {
    let index = ObjectIndex {
        busy: periods,
        schedule_tag: Some("st".into()),
        ..ObjectIndex::new(
            Component::Event,
            name,
            Some(Span {
                start: at(0),
                end: None,
            }),
        )
    };
    let out = meta
        .put_object(
            scope,
            cal.id,
            name,
            "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".into(),
            index,
            Precondition::Any,
        )
        .await
        .expect("put");
    assert!(matches!(out, PutOutcome::Stored(_)));
}

#[tokio::test]
async fn passes_freebusy_suite() {
    let Some((meta, fx)) = fixture().await else {
        return;
    };
    suite::freebusy(&meta, &meta, &fx).await;
}

/// META-54: еженедельная встреча без конца видна через 3 года, исключённая дата не занята.
#[tokio::test]
async fn passes_freebusy_recurring_suite() {
    let Some((meta, fx)) = fixture().await else {
        return;
    };
    suite::freebusy_recurring(&meta, &meta, &fx).await;
}

#[tokio::test]
async fn passes_itip_suite() {
    let Some((meta, fx)) = fixture().await else {
        return;
    };
    suite::itip(&meta, &fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbox_signals_after_commit() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let bus = Arc::new(MemEventBus::default());
    let meta = PgMeta::new(pool, MetaConfig::default()).with_bus(bus.clone());
    let scope = common::account(&meta).await;
    suite::itip::signals(&meta, bus.as_ref(), scope).await;
}

/// Перезапись объекта заменяет индекс занятости, удаление объекта и коллекции — убирает его строки.
#[tokio::test]
async fn busy_rows_follow_object() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let cal = Collection::new(CollectionKind::Calendar, "work");
    meta.create_collection(scope, cal.clone())
        .await
        .expect("create");
    let rows = async || -> i64 {
        let mut tx = schema::begin(meta.pool(), scope.tenant)
            .await
            .expect("begin");
        let n = sqlx::query_scalar("SELECT count(*) FROM dav_busy WHERE account_id = $1")
            .bind(scope.account.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .expect("count");
        tx.rollback().await.expect("rollback");
        n
    };
    // Одинаковые периоды в индексе допустимы и читаются как записаны.
    let twice = vec![busy(1, 2), busy(1, 2), busy(5, 6)];
    put_event(&meta, scope, &cal, "a", twice.clone()).await;
    put_event(&meta, scope, &cal, "b", vec![busy(8, 9)]).await;
    assert_eq!(rows().await, 4);
    let got = meta
        .get_object(scope, cal.id, "a")
        .await
        .expect("get")
        .expect("a");
    assert_eq!(got.index.busy, twice);
    let by_id = meta
        .objects_by_id(scope, &[got.id])
        .await
        .expect("objects_by_id");
    assert_eq!(by_id[0].index, got.index);

    put_event(&meta, scope, &cal, "a", vec![busy(3, 4)]).await;
    assert_eq!(rows().await, 2);
    put_event(&meta, scope, &cal, "a", Vec::new()).await;
    assert_eq!(rows().await, 1);
    let got = meta
        .get_object(scope, cal.id, "a")
        .await
        .expect("get")
        .expect("a");
    assert!(got.index.busy.is_empty());
    meta.delete_object(scope, cal.id, "b", Precondition::Any)
        .await
        .expect("delete");
    assert_eq!(rows().await, 0);
    put_event(&meta, scope, &cal, "c", vec![busy(1, 2)]).await;
    meta.delete_collection(scope, cal.id)
        .await
        .expect("delete collection");
    assert_eq!(rows().await, 0);
    let period = Period {
        start: at(0),
        end: at(48),
    };
    assert_eq!(
        meta.query(scope.tenant, &[scope.account], period).await,
        Ok(vec![(scope.account, Vec::new())])
    );
}

/// Чужая организация не видит строк занятости и входящих (RLS, роль узлов `thepost_app`).
#[tokio::test]
async fn rls_hides_other_tenants() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let owner = common::account(&meta).await;
    let cal = Collection::new(CollectionKind::Calendar, "work");
    meta.create_collection(owner, cal.clone())
        .await
        .expect("create");
    put_event(&meta, owner, &cal, "a", vec![busy(1, 2)]).await;
    meta.inbox_put(owner, "a.ics", message("u1"))
        .await
        .expect("inbox_put");
    let other = common::tenant(meta.pool()).await;
    let counts = async |tenant: TenantId| -> Vec<i64> {
        let mut tx = schema::begin(meta.pool(), tenant).await.expect("begin");
        sqlx::query("SET LOCAL ROLE thepost_app")
            .execute(&mut *tx)
            .await
            .expect("role");
        let mut out = Vec::new();
        for table in ["dav_busy", "dav_inbox"] {
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
            "INSERT INTO dav_inbox (tenant_id, account_id, item_id, name, method, uid, sender, body, etag, modseq)
             VALUES ($1, $2, $3, 'x.ics', 'REQUEST', 'u', 'a@b.example', '', '1', 1)",
        )
        .bind(if tenant == owner.tenant {
            other.as_uuid()
        } else {
            owner.tenant.as_uuid()
        })
        .bind(owner.account.as_uuid())
        .bind(uuid::Uuid::now_v7())
        .execute(&mut *tx)
        .await
        .expect_err("чужая организация");
        assert!(matches!(schema::db_err(err), Error::Forbidden));
        tx.rollback().await.expect("rollback");
        out
    };
    assert_eq!(counts(owner.tenant).await, [1, 1]);
    assert_eq!(counts(other).await, [0, 0]);
}

/// Миграция 0024 на базе прежней схемы: календарь и событие, записанные до миграции, читаются — без занятости,
/// `Schedule-Tag` и прозрачности; после миграции планирование работает.
#[tokio::test]
async fn migration_on_existing_db() {
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
    schema::migrator_until(23)
        .run(&pool)
        .await
        .expect("migrate until");
    let pool = common::db(pool);
    let tenant = common::tenant(&pool).await;
    let (domain, _) = common::domain(&pool, tenant).await;
    let scope = AccountScope {
        tenant,
        account: AccountId::new(),
    };
    let cal = Collection::new(CollectionKind::Calendar, "work");
    let object = uuid::Uuid::now_v7();
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
    sqlx::query(
        "INSERT INTO dav_collections (tenant_id, account_id, collection_id, kind, name, display_name, components,
                                      created_modseq, props_modseq, modseq, object_count)
         VALUES ($1, $2, $3, 'calendar', 'work', 'work', ARRAY['VEVENT'], 1, 1, 2, 1)",
    )
    .bind(tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(cal.id.as_uuid())
    .execute(&mut *tx)
    .await
    .expect("collection");
    sqlx::query(
        "INSERT INTO dav_objects (tenant_id, account_id, object_id, collection_id, name, uid, etag, component,
                                  span_start, body, created_modseq, modseq)
         VALUES ($1, $2, $3, $4, 'old.ics', 'old', '2', 'VEVENT', $5, 'BODY', 2, 2)",
    )
    .bind(tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(object)
    .bind(cal.id.as_uuid())
    .bind(at(0))
    .execute(&mut *tx)
    .await
    .expect("object");
    sqlx::query(
        "UPDATE mailbox_state SET modseq = 2, dav_calendar_state = 2 WHERE account_id = $1",
    )
    .bind(scope.account.as_uuid())
    .execute(&mut *tx)
    .await
    .expect("state");
    tx.commit().await.expect("commit");

    schema::migrate(&pool).await.expect("migrate");
    let meta = PgMeta::new(pool.clone(), MetaConfig::default());
    let old = meta
        .get_object(scope, cal.id, "old.ics")
        .await
        .expect("get")
        .expect("old");
    assert!(old.index.busy.is_empty() && old.index.schedule_tag.is_none());
    let coll = meta
        .get_collection(scope, cal.id)
        .await
        .expect("get")
        .expect("work");
    assert!(!coll.transparent);
    let inbox = meta.inbox_list(scope).await.expect("inbox_list");
    assert!(inbox.items.is_empty());
    assert_eq!(inbox.state, ModSeq::ZERO);
    let period = Period {
        start: at(0),
        end: at(48),
    };
    assert_eq!(
        meta.query(tenant, &[scope.account], period).await,
        Ok(vec![(scope.account, Vec::new())]),
        "занятость прежних объектов появится после их перезаписи"
    );
    // Перезапись прежнего объекта заполняет индекс; входящие — из общего счётчика.
    put_event(&meta, scope, &cal, "old.ics", vec![busy(1, 2)]).await;
    assert_eq!(
        meta.query(tenant, &[scope.account], period).await,
        Ok(vec![(scope.account, vec![busy(1, 2)])])
    );
    let put = meta
        .inbox_put(scope, "a.ics", message("u1"))
        .await
        .expect("inbox_put");
    assert_eq!(put.modseq, ModSeq(4));
    assert_eq!(
        meta.state(scope, CollectionKind::Calendar).await,
        Ok(ModSeq(3))
    );
    pool.close().await;
    admin
        .execute(AssertSqlSafe(format!("DROP DATABASE {name} WITH (FORCE)")))
        .await
        .expect("drop database");
}

/// Миграция 0025 на базе схемы META-51: у события с `RRULE` периоды, записанные в индекс до горизонта, убираются,
/// событие помечается повторяющимся и разворачивается при запросе; неповторяющееся остаётся в индексе.
#[tokio::test]
async fn migration_moves_recurring_out_of_index() {
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
    schema::migrator_until(24)
        .run(&pool)
        .await
        .expect("migrate until");
    let pool = common::db(pool);
    let tenant = common::tenant(&pool).await;
    let (domain, _) = common::domain(&pool, tenant).await;
    let scope = AccountScope {
        tenant,
        account: AccountId::new(),
    };
    let cal = Collection::new(CollectionKind::Calendar, "work");
    let ts = |unix: i64| OffsetDateTime::from_unix_timestamp(unix).expect("метка времени");
    // Понедельник 5 января 2026, 10:00 UTC, каждую неделю без конца; разовая встреча — 6 января.
    let first = 1_767_607_200;
    let weekly = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:weekly\r\n\
                  DTSTAMP:20260101T000000Z\r\nDTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\n\
                  rrule:FREQ=WEEKLY\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    // «RRULE» в тексте описания — не правило повторения.
    let single = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:single\r\n\
                  DTSTAMP:20260101T000000Z\r\nDTSTART:20260106T100000Z\r\nDTEND:20260106T110000Z\r\n\
                  DESCRIPTION:no RRULE:here\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let (weekly_id, single_id) = (uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
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
    sqlx::query(
        "INSERT INTO dav_collections (tenant_id, account_id, collection_id, kind, name, display_name, components,
                                      created_modseq, props_modseq, modseq, object_count)
         VALUES ($1, $2, $3, 'calendar', 'work', 'work', ARRAY['VEVENT'], 1, 1, 3, 2)",
    )
    .bind(tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(cal.id.as_uuid())
    .execute(&mut *tx)
    .await
    .expect("collection");
    for (id, uid, body, start, end, ms) in [
        (weekly_id, "weekly", weekly, first, None, 2_i64),
        (
            single_id,
            "single",
            single,
            first + 86_400,
            Some(first + 90_000),
            3,
        ),
    ] {
        sqlx::query(
            "INSERT INTO dav_objects (tenant_id, account_id, object_id, collection_id, name, uid, etag, component,
                                      span_start, span_end, body, created_modseq, modseq)
             VALUES ($1, $2, $3, $4, $5 || '.ics', $5, '1', 'VEVENT', $6, $7, $8, $9, $9)",
        )
        .bind(tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(id)
        .bind(cal.id.as_uuid())
        .bind(uid)
        .bind(ts(start))
        .bind(end.map(ts))
        .bind(body)
        .bind(ms)
        .execute(&mut *tx)
        .await
        .expect("object");
    }
    // Индекс META-51: у еженедельной — вхождения до горизонта (здесь два), у разовой — её период.
    for (id, ord, start) in [
        (weekly_id, 0, first),
        (weekly_id, 1, first + 7 * 86_400),
        (single_id, 0, first + 86_400),
    ] {
        sqlx::query(
            "INSERT INTO dav_busy (tenant_id, account_id, object_id, ord, collection_id, busy_start, busy_end, kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'busy')",
        )
        .bind(tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(id)
        .bind(ord)
        .bind(cal.id.as_uuid())
        .bind(ts(start))
        .bind(ts(start + 3600))
        .execute(&mut *tx)
        .await
        .expect("busy");
    }
    sqlx::query(
        "UPDATE mailbox_state SET modseq = 3, dav_calendar_state = 3 WHERE account_id = $1",
    )
    .bind(scope.account.as_uuid())
    .execute(&mut *tx)
    .await
    .expect("state");
    tx.commit().await.expect("commit");

    schema::migrate(&pool).await.expect("migrate");
    let meta = PgMeta::new(pool.clone(), MetaConfig::default());
    let w = meta
        .get_object(scope, cal.id, "weekly.ics")
        .await
        .expect("get")
        .expect("weekly");
    assert!(w.index.recurring && w.index.busy.is_empty());
    let s = meta
        .get_object(scope, cal.id, "single.ics")
        .await
        .expect("get")
        .expect("single");
    assert!(!s.index.recurring);
    assert_eq!(s.index.busy.len(), 1);
    let hour = |unix: i64| BusyPeriod {
        start: ts(unix),
        end: ts(unix + 3600),
        kind: BusyKind::Busy,
    };
    // Первая неделя: еженедельная — разворотом, разовая — из индекса; без двойного счёта.
    assert_eq!(
        meta.query(
            tenant,
            &[scope.account],
            Period {
                start: ts(first - 3600),
                end: ts(first + 2 * 86_400),
            }
        )
        .await,
        Ok(vec![(
            scope.account,
            vec![hour(first), hour(first + 86_400)]
        )])
    );
    // Через 3 года (за прежним горизонтом в 400 дней): понедельник 5 февраля 2029.
    assert_eq!(
        meta.query(
            tenant,
            &[scope.account],
            Period {
                start: ts(1_864_598_400),
                end: ts(1_864_598_400 + 7 * 86_400),
            }
        )
        .await,
        Ok(vec![(scope.account, vec![hour(1_864_980_000)])])
    );
    pool.close().await;
    admin
        .execute(AssertSqlSafe(format!("DROP DATABASE {name} WITH (FORCE)")))
        .await
        .expect("drop database");
}

/// Занятость и входящие учётной записи второго шарда — в БД этого шарда; запрос занятости собирает учётные
/// записи обоих шардов.
#[tokio::test]
async fn schedule_lives_on_account_shard() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let tenant = t.tenant().await;
    let on_main = t
        .account_on(tenant.clone(), tp_meta::shards::ShardId::MAIN)
        .await;
    let on_shard = t.account_on(tenant.clone(), t.shard).await;
    let cal = Collection::new(CollectionKind::Calendar, "work");
    for (scope, period) in [(on_main, busy(1, 2)), (on_shard, busy(3, 4))] {
        t.meta
            .create_collection(scope, Collection { ..cal.clone() })
            .await
            .expect("create");
        put_event(&t.meta, scope, &cal, "a", vec![period]).await;
    }
    t.meta
        .inbox_put(on_shard, "a.ics", message("u1"))
        .await
        .expect("inbox_put");
    for table in ["dav_busy", "dav_inbox"] {
        let count = async |pool: &tp_meta::pool::DbPool| -> i64 {
            let mut tx = schema::begin_system(pool).await.expect("begin");
            let n = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT count(*) FROM {table} WHERE account_id = $1"
            )))
            .bind(on_shard.account.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .expect("count");
            tx.rollback().await.expect("rollback");
            n
        };
        assert_eq!(count(&t.shard_pool).await, 1, "{table}");
        assert_eq!(count(t.main.meta.pool()).await, 0, "{table}");
    }
    assert_eq!(
        t.meta
            .inbox_list(on_shard)
            .await
            .expect("inbox_list")
            .items
            .len(),
        1
    );
    let nobody = AccountId::new();
    let got = t
        .meta
        .query(
            tenant.0,
            &[on_shard.account, nobody, on_main.account],
            Period {
                start: at(0),
                end: at(48),
            },
        )
        .await
        .expect("query");
    assert_eq!(
        got,
        vec![
            (on_shard.account, vec![busy(3, 4)]),
            (nobody, Vec::new()),
            (on_main.account, vec![busy(1, 2)]),
        ]
    );
    t.drop_db().await;
}
