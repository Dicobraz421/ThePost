//! META-01: миграции на чистую БД, идемпотентность, RLS по организации.

mod common;

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Executor};
use tp_contracts::{Error, TenantId};
use tp_meta::schema;

#[tokio::test]
async fn migrations_apply_on_clean_db_and_are_idempotent() {
    let Some(url) = common::pg_url() else { return };
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect");
    let db = format!("tp_meta_{}", uuid::Uuid::now_v7().simple());
    admin
        .execute(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .await
        .expect("create database");

    let opts = PgConnectOptions::from_str(&url).expect("url").database(&db);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .expect("connect clean");
    schema::migrate(&pool).await.expect("first");
    schema::migrate(&pool).await.expect("second");
    // Параллельный старт нескольких узлов.
    let runs: Vec<_> = (0..3)
        .map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move { schema::migrate(&pool).await })
        })
        .collect();
    for r in runs {
        r.await.expect("join").expect("concurrent");
    }

    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(applied, schema::migrator().iter().count() as i64);

    let mut tx = schema::begin(&pool, TenantId::DEFAULT)
        .await
        .expect("begin");
    let name: String = sqlx::query_scalar("SELECT name FROM tenants")
        .fetch_one(&mut *tx)
        .await
        .expect("default tenant");
    assert_eq!(name, "default");
    tx.rollback().await.expect("rollback");
    pool.close().await;

    admin
        .execute(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .await
        .expect("drop database");
}

#[tokio::test]
async fn rls_hides_other_tenants() {
    let Some(pool) = common::pool().await else {
        return;
    };
    // Суперпользователь обходит RLS: проверяем от имени обычной роли.
    pool.execute(
        "DO $$ BEGIN CREATE ROLE tp_meta_rls NOLOGIN; EXCEPTION WHEN duplicate_object THEN NULL; END $$;
         GRANT USAGE ON SCHEMA public TO tp_meta_rls;
         GRANT SELECT, INSERT ON ALL TABLES IN SCHEMA public TO tp_meta_rls;",
    )
    .await
    .expect("role");

    let a = common::tenant(&pool).await;
    let b = common::tenant(&pool).await;
    let (_, da) = common::domain(&pool, a).await;
    let (_, db) = common::domain(&pool, b).await;
    let names = vec![da.clone(), db.clone()];
    let count = "SELECT count(*) FROM domains WHERE name = ANY($1)";

    // Без организации — ни одной строки.
    let mut tx = pool.begin().await.expect("begin");
    tx.execute("SET LOCAL ROLE tp_meta_rls")
        .await
        .expect("role");
    let n: i64 = sqlx::query_scalar(count)
        .bind(&names)
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    assert_eq!(n, 0);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&mut *tx)
        .await
        .expect("tenants");
    assert_eq!(n, 0);
    tx.rollback().await.expect("rollback");

    // В контексте организации A — только её строки; запись в чужую организацию запрещена.
    let mut tx = schema::begin(&pool, a).await.expect("begin");
    tx.execute("SET LOCAL ROLE tp_meta_rls")
        .await
        .expect("role");
    let visible: Vec<String> = sqlx::query_scalar("SELECT name FROM domains WHERE name = ANY($1)")
        .bind(&names)
        .fetch_all(&mut *tx)
        .await
        .expect("select");
    assert_eq!(visible, vec![da]);
    let err = sqlx::query("INSERT INTO domains (tenant_id, domain_id, name) VALUES ($1, $2, $3)")
        .bind(b.as_uuid())
        .bind(uuid::Uuid::now_v7())
        .bind(format!("x{}", db))
        .execute(&mut *tx)
        .await
        .expect_err("чужая организация");
    let code = err
        .as_database_error()
        .and_then(|e| e.code())
        .map(|c| c.into_owned());
    assert_eq!(code.as_deref(), Some("42501"));
    tx.rollback().await.expect("rollback");

    // Системная транзакция видит все организации.
    let mut tx = schema::begin_system(&pool).await.expect("begin");
    tx.execute("SET LOCAL ROLE tp_meta_rls")
        .await
        .expect("role");
    let n: i64 = sqlx::query_scalar(count)
        .bind(&names)
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    assert_eq!(n, 2);
    tx.rollback().await.expect("rollback");
}

#[tokio::test]
async fn unknown_tenant_insert_is_rejected() {
    let Some(pool) = common::pool().await else {
        return;
    };
    // Домен несуществующей организации: нарушение внешнего ключа → Invalid.
    let ghost = TenantId::new();
    let mut tx = schema::begin(&pool, ghost).await.expect("begin");
    let err = sqlx::query("INSERT INTO domains (tenant_id, domain_id, name) VALUES ($1, $2, $3)")
        .bind(ghost.as_uuid())
        .bind(uuid::Uuid::now_v7())
        .bind(format!("g{}.test", ghost.as_uuid().simple()))
        .execute(&mut *tx)
        .await
        .expect_err("fk");
    assert!(matches!(schema::db_err(err), Error::Invalid(_)));
}
