//! CTR-44, META-45: постраничный список учётных записей на PostgreSQL — общий набор `tp_testkit::suite::account_list`
//! (побайтный порядок, страницы, курсор, префикс, регистр, изоляция организаций) и план запроса по индексу
//! `accounts_login_c`; набор `account_by_ids` (CTR-63, META-66; одним запросом); сигнал `AccountDisabled` (META-67);
//! набор `account_by_address` (CTR-62, META-68; псевдонимы и группы одним запросом); роль администратора
//! установки — набор `account_installation_admin` и перенос роли миграцией 0040 (решение 117).

mod common;

use std::str::FromStr;
use std::sync::Arc;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Executor};
use time::{Duration, OffsetDateTime};
use tp_contracts::bus::{Event, EventKind};
use tp_contracts::meta::{
    AccountKind, AccountScope, AccountStore, AccountUpsert, account_disabled,
};
use tp_contracts::{AccountId, TenantId};
use tp_meta::accounts::{LIST_PREFIX_SQL, LIST_SQL};
use tp_meta::{MetaConfig, PgMeta, schema};
use tp_testkit::suite::{
    AccountListFixture, account_by_address, account_by_ids, account_installation_admin,
    account_list,
};

#[tokio::test]
async fn passes_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    // Набор сравнивает с `str::to_lowercase` хранилища в памяти: `lower` базы для его не-ASCII букв — то же.
    let upper = "ÉMILE ЮРИЙ";
    let (lowered, ctype): (String, String) = sqlx::query_as(
        "SELECT lower($1), datctype::text FROM pg_database WHERE datname = current_database()",
    )
    .bind(upper)
    .fetch_one(&mut *common::conn(meta.pool()).await)
    .await
    .expect("lower");
    assert_eq!(
        lowered,
        upper.to_lowercase(),
        "lower базы (LC_CTYPE {ctype})"
    );

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

/// CTR-63, META-66: `AccountStore::by_ids` одним запросом — порядок запроса, пропуск несуществующих и чужих.
#[tokio::test]
async fn passes_by_ids_suite() {
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
    account_by_ids(&meta, &fx).await;
}

/// CTR-62, META-68: `AccountStore::by_address` — основной адрес, псевдоним, группа рассылки; чужая организация не видна.
#[tokio::test]
async fn passes_by_address_suite() {
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
    account_by_address(&meta, &meta, &fx).await;
}

/// META-67: переход `enabled` в `false` (`upsert`, синхронизация каталога) и `disable_account` публикуют
/// `AccountDisabled` после фиксации; новая запись, повторное сохранение без изменения и разрешение — без сигнала.
#[tokio::test]
async fn disabling_publishes_account_disabled() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let rec = Arc::new(common::RecordingBus::default());
    let meta = meta.with_bus(rec.clone());
    let tenant = common::tenant(meta.pool()).await;
    let (domain, domain_name) = common::domain(meta.pool(), tenant).await;
    let disabled = || -> Vec<Event> {
        rec.take()
            .into_iter()
            .filter(|e| matches!(e.kind, EventKind::AccountDisabled { .. }))
            .collect()
    };
    let data = |enabled: bool| AccountUpsert {
        domain,
        login: format!("block@{domain_name}"),
        display_name: "Блок".into(),
        external_id: Some("ext-block".into()),
        enabled,
        aliases: Vec::new(),
        kind: AccountKind::User,
        hidden_from_gal: false,
    };

    let a = meta.upsert(tenant, data(true)).await.expect("create");
    assert_eq!(disabled(), Vec::new(), "новая запись");
    let scope = AccountScope {
        tenant,
        account: a.id,
    };

    let off = meta.upsert(tenant, data(false)).await.expect("disable");
    assert!(!off.enabled);
    assert_eq!(disabled(), vec![account_disabled(scope)]);
    meta.upsert(tenant, data(false)).await.expect("same");
    assert_eq!(disabled(), Vec::new(), "повтор без изменения");
    meta.upsert(tenant, data(true)).await.expect("enable");
    assert_eq!(disabled(), Vec::new(), "разрешение");

    // Новая запрещённая — входа ещё не было.
    let other = AccountUpsert {
        login: format!("off@{domain_name}"),
        external_id: None,
        ..data(false)
    };
    meta.upsert(tenant, other).await.expect("create disabled");
    assert_eq!(disabled(), Vec::new(), "новая запрещённая");

    let until = OffsetDateTime::now_utc() + Duration::days(30);
    meta.disable_account(tenant, a.id, until)
        .await
        .expect("disable_account");
    assert_eq!(disabled(), vec![account_disabled(scope)]);
    meta.restore_account(tenant, a.id).await.expect("restore");
    assert_eq!(disabled(), Vec::new(), "восстановление");
}

/// Оба запроса списка идут по `accounts_login_c` без сортировки; префикс — граница индекса (`Index Cond`).
#[tokio::test]
async fn list_uses_byte_order_index() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let plan = |sql: &'static str, prefix_next: Option<&'static str>| {
        let pool = meta.pool().clone();
        async move {
            let mut tx = schema::begin(&pool, tenant).await.expect("begin");
            // Таблица в тесте мала: без запрета полного просмотра планировщик индекс не выберет.
            sqlx::query("SET LOCAL enable_seqscan = off")
                .execute(&mut *tx)
                .await
                .expect("enable_seqscan");
            let mut q = sqlx::query_scalar(AssertSqlSafe(format!("EXPLAIN {sql}")))
                .bind(tenant.as_uuid())
                .bind("e")
                .bind("")
                .bind(10_i64);
            if let Some(next) = prefix_next {
                q = q.bind(next);
            }
            let lines: Vec<String> = q.fetch_all(&mut *tx).await.expect("explain");
            lines.join("\n")
        }
    };
    for (sql, next) in [(LIST_PREFIX_SQL, Some("f")), (LIST_SQL, None)] {
        let plan = plan(sql, next).await;
        assert!(plan.contains("Index Scan using accounts_login_c"), "{plan}");
        assert!(!plan.contains("Sort"), "{plan}");
        let cond = plan
            .lines()
            .find(|l| l.contains("Index Cond"))
            .unwrap_or_default();
        assert!(cond.contains(">="), "{plan}");
        // Без границы планировщик сам выводит её из `starts_with` для константы (C), с границей — наша `< $5`.
        if next.is_some() {
            assert!(cond.contains("< 'f'"), "{plan}");
        }
    }
}

/// Решение 117: роль администратора установки — запись `installation_admins`; изоляция организаций.
#[tokio::test]
async fn passes_installation_admin_suite() {
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
    account_installation_admin(&meta, &fx).await;
}

/// Миграция 0040 на базе прежней схемы: учётная запись `admin init` (действующий пароль приложения с подписью
/// `thepost admin init` в организации по умолчанию) получает запись о роли; отозванный пароль, другая подпись и другая
/// организация — нет. Пароли приложений не удаляются.
#[tokio::test]
async fn installation_admin_migration_on_existing_db() {
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
    schema::migrator_until(39)
        .run(&pool)
        .await
        .expect("migrate until");
    let pool = common::db(pool);
    let default = TenantId::DEFAULT;
    let other = common::tenant(&pool).await;
    let (default_domain, _) = common::domain(&pool, default).await;
    let (other_domain, _) = common::domain(&pool, other).await;
    // (организация, домен, вход, подпись пароля, отозван)
    let rows = [
        (default, default_domain, "root", "thepost admin init", false),
        (
            default,
            default_domain,
            "revoked",
            "thepost admin init",
            true,
        ),
        (default, default_domain, "user", "phone", false),
        (other, other_domain, "foreign", "thepost admin init", false),
    ];
    let mut ids = Vec::new();
    for (i, (tenant, domain, login, label, revoked)) in rows.iter().enumerate() {
        let id = AccountId::new();
        let mut tx = schema::begin(&pool, *tenant).await.expect("begin");
        sqlx::query(
            "INSERT INTO accounts (tenant_id, account_id, domain_id, login) VALUES ($1, $2, $3, $4)",
        )
        .bind(tenant.as_uuid())
        .bind(id.as_uuid())
        .bind(domain.as_uuid())
        .bind(format!("{login}@example.org"))
        .execute(&mut *tx)
        .await
        .expect("account");
        sqlx::query(
            "INSERT INTO app_passwords (tenant_id, account_id, password_id, label, hash, created_at, revoked_at)
             VALUES ($1, $2, $3, $4, 'hash', now(), CASE WHEN $5 THEN now() END)",
        )
        .bind(tenant.as_uuid())
        .bind(id.as_uuid())
        .bind(format!("p{i}"))
        .bind(label)
        .bind(revoked)
        .execute(&mut *tx)
        .await
        .expect("app password");
        tx.commit().await.expect("commit");
        ids.push((*tenant, id));
    }

    schema::migrate(&pool).await.expect("migrate");
    let meta = PgMeta::new(pool.clone(), MetaConfig::default());
    let mut roles = Vec::new();
    for (tenant, id) in &ids {
        roles.push(
            meta.is_installation_admin(*tenant, *id)
                .await
                .expect("is_installation_admin"),
        );
    }
    assert_eq!(roles, [true, false, false, false]);
    let mut tx = schema::begin(&pool, default).await.expect("begin");
    let (passwords,): (i64,) = sqlx::query_as("SELECT count(*) FROM app_passwords")
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    tx.commit().await.expect("commit");
    assert_eq!(passwords, 3, "пароли организации по умолчанию остаются");
    pool.close().await;
    admin
        .execute(AssertSqlSafe(format!("DROP DATABASE {name} WITH (FORCE)")))
        .await
        .expect("drop database");
}
