//! CTR-44, META-45: постраничный список учётных записей на PostgreSQL — общий набор `tp_testkit::suite::account_list`
//! (побайтный порядок, страницы, курсор, префикс, регистр, изоляция организаций) и план запроса по индексу
//! `accounts_login_c`.

mod common;

use sqlx::AssertSqlSafe;
use tp_meta::accounts::{LIST_PREFIX_SQL, LIST_SQL};
use tp_meta::schema;
use tp_testkit::suite::{AccountListFixture, account_list};

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
