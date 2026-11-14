//! Брендинг заказчика на PostgreSQL (META-84; CTR-91): набор `tp-testkit` и миграция 0043. Без `TEST_DATABASE_URL`
//! тесты пропускаются.

mod common;

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Executor};
use tp_contracts::Error;
use tp_contracts::branding::{Branding, BrandingScope, BrandingStore, Role};
use tp_meta::{MetaConfig, PgMeta, schema};
use tp_testkit::suite;

/// Набор на своей чистой БД: уровень установки — одна строка на всю БД, параллельные тесты на общей БД его бы
/// делили.
#[tokio::test]
async fn branding_passes_contract_suite() {
    let Some(db) = common::fresh_meta().await else {
        return;
    };
    let meta = &db.meta;
    let org = common::tenant(meta.pool()).await;
    let foreign = common::tenant(meta.pool()).await;
    suite::branding(meta, org, foreign).await;
    db.drop_db().await;
}

/// Миграция 0043 поверх 0042: таблица создаётся, повторный запуск ничего не меняет; вторая строка уровня установки
/// и неверный цвет — ошибки ограничений; набор организации другой организации не виден (RLS), набор установки —
/// виден; удаление организации удаляет её набор.
#[tokio::test]
async fn branding_migration_on_existing_db() {
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
    schema::migrator_until(42)
        .run(&pool)
        .await
        .expect("migrate until");
    let pool = common::db(pool);
    let org = common::tenant(&pool).await;
    let foreign = common::tenant(&pool).await;

    schema::migrate(&pool).await.expect("migrate");
    let tables = async || -> i64 {
        let mut tx = schema::begin_system(&pool).await.expect("begin");
        let n = sqlx::query_scalar(
            "SELECT count(*) FROM pg_tables WHERE schemaname = 'public' AND tablename = 'branding'",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("tables");
        tx.commit().await.expect("commit");
        n
    };
    assert_eq!(tables().await, 1);
    schema::migrate(&pool).await.expect("повторный запуск");
    assert_eq!(tables().await, 1);

    let insert = async |tenant: Option<uuid::Uuid>, color: &str| {
        let mut tx = schema::begin_system(&pool).await.expect("begin");
        let r = sqlx::query("INSERT INTO branding (tenant_id, primary_color) VALUES ($1, $2)")
            .bind(tenant)
            .bind(color)
            .execute(&mut *tx)
            .await;
        if r.is_ok() {
            tx.commit().await.expect("commit");
        }
        r.map(|_| ()).map_err(schema::db_err)
    };
    insert(None, "#177CAA").await.expect("installation");
    assert_eq!(
        insert(None, "#2E7D32").await,
        Err(Error::Conflict),
        "вторая строка уровня установки"
    );
    assert!(matches!(
        insert(Some(*org.as_uuid()), "#177caa").await,
        Err(Error::Invalid(_))
    ));

    let meta = PgMeta::new(pool.clone(), MetaConfig::default());
    let mut own = Branding {
        product_name: Some("Ромашка".into()),
        primary: Some("#2E7D32".parse().expect("primary")),
        ..Branding::default()
    };
    own.dark
        .insert(Role::Head, "#2EBCBA".parse().expect("head"));
    meta.set_branding(BrandingScope::Org(org), &own)
        .await
        .expect("set org");
    assert_eq!(
        meta.branding(BrandingScope::Org(org)).await,
        Ok(Some(own.clone())),
        "роли — из jsonb как заданы"
    );
    // RLS действует на роль узлов `thepost_app` (владелец тестовой БД её обходит).
    let as_app = async |tx: &mut sqlx::Transaction<'static, sqlx::Postgres>| {
        sqlx::query("SET LOCAL ROLE thepost_app")
            .execute(&mut **tx)
            .await
            .expect("role");
    };
    let visible = async |tenant| -> Vec<Option<uuid::Uuid>> {
        let mut tx = schema::begin(&pool, tenant).await.expect("begin");
        as_app(&mut tx).await;
        let rows =
            sqlx::query_scalar("SELECT tenant_id FROM branding ORDER BY tenant_id NULLS FIRST")
                .fetch_all(&mut *tx)
                .await
                .expect("select");
        tx.commit().await.expect("commit");
        rows
    };
    assert_eq!(visible(org).await, vec![None, Some(*org.as_uuid())]);
    assert_eq!(visible(foreign).await, vec![None], "чужой набор скрыт");

    // Транзакция организации не меняет набор установки.
    let mut tx = schema::begin(&pool, foreign).await.expect("begin");
    as_app(&mut tx).await;
    let deleted = sqlx::query("DELETE FROM branding WHERE tenant_id IS NULL")
        .execute(&mut *tx)
        .await
        .expect("delete")
        .rows_affected();
    tx.commit().await.expect("commit");
    assert_eq!(deleted, 0);

    let mut tx = schema::begin_system(&pool).await.expect("begin");
    sqlx::query("DELETE FROM tenants WHERE tenant_id = $1")
        .bind(org.as_uuid())
        .execute(&mut *tx)
        .await
        .expect("delete tenant");
    tx.commit().await.expect("commit");
    assert_eq!(meta.branding(BrandingScope::Org(org)).await, Ok(None));
    assert!(
        meta.branding(BrandingScope::Installation)
            .await
            .expect("installation")
            .is_some()
    );

    pool.close().await;
    admin
        .execute(AssertSqlSafe(format!("DROP DATABASE {name} WITH (FORCE)")))
        .await
        .expect("drop database");
}
