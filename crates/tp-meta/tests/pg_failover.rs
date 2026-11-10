//! Поиск primary и переключение СУБД (META-25): две БД одного сервера — два «экземпляра», перевод primary
//! в режим только для чтения с обрывом сеансов (как при перезапуске бывшего primary репликой).

mod common;

use std::collections::HashSet;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, Executor, PgPool};
use tp_contracts::TenantId;
use tp_meta::failover::{FailoverConfig, connect_failover};
use tp_meta::schema::db_err;
use uuid::Uuid;

const APP: &str = "tp_failover_test";

/// Адрес `url` с БД `db`.
fn url_for(url: &str, db: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(b, q)| (b, Some(q)));
    let rest = base.split_once("://").map_or(base, |(_, r)| r);
    let base = match rest.find('/') {
        Some(i) => &base[..base.len() - rest.len() + i],
        None => base,
    };
    match query {
        Some(q) => format!("{base}/{db}?{q}"),
        None => format!("{base}/{db}"),
    }
}

async fn create_db(admin: &PgPool, url: &str, name: &str) {
    admin
        .execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .await
        .expect("create database");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url_for(url, name))
        .await
        .expect("connect");
    pool.execute("CREATE TABLE writes (id uuid PRIMARY KEY)")
        .await
        .expect("table");
    pool.close().await;
}

async fn ids(url: &str, name: &str) -> HashSet<Uuid> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url_for(url, name))
        .await
        .expect("connect");
    let rows: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM writes")
        .fetch_all(&pool)
        .await
        .expect("select");
    pool.close().await;
    rows.into_iter().collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_primary_switches_without_losing_writes() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("подключение к TEST_DATABASE_URL");
    let tag = Uuid::now_v7().simple().to_string();
    let (a, b) = (format!("tp_fo_a_{tag}"), format!("tp_fo_b_{tag}"));
    create_db(&admin, &url, &a).await;
    create_db(&admin, &url, &b).await;

    let mut cfg = common::dev_pool_config(&url_for(&url, &a));
    cfg.urls = vec![common::dev_url(
        &common::split_password(&url_for(&url, &b)).0,
    )];
    cfg.application_name = APP.into();
    cfg.failover = FailoverConfig {
        retry_initial: Duration::from_millis(20),
        retry_max: Duration::from_millis(200),
        retry_for: Duration::from_secs(20),
        probe_timeout: Duration::from_secs(2),
    };
    let fo = connect_failover(&cfg, common::secrets())
        .await
        .expect("connect_failover");
    assert_eq!(fo.metrics().primary, 0, "primary — первый адрес");

    let tenant = TenantId::new();
    let mut writers = Vec::new();
    for _ in 0..4 {
        let fo = fo.clone();
        writers.push(tokio::spawn(async move {
            let mut done = Vec::new();
            for _ in 0..50 {
                let id = Uuid::now_v7();
                fo.with_tenant_keyed(
                    tenant,
                    move |tx| {
                        Box::pin(async move {
                            sqlx::query(
                                "INSERT INTO writes (id) VALUES ($1) ON CONFLICT DO NOTHING",
                            )
                            .bind(id)
                            .execute(&mut *tx)
                            .await
                            .map(drop)
                            .map_err(db_err)
                        })
                    },
                    move |tx| {
                        Box::pin(async move {
                            let found: bool = sqlx::query_scalar(
                                "SELECT EXISTS (SELECT 1 FROM writes WHERE id = $1)",
                            )
                            .bind(id)
                            .fetch_one(&mut *tx)
                            .await
                            .map_err(db_err)?;
                            Ok(found.then_some(()))
                        })
                    },
                )
                .await
                .expect("запись переживает переключение");
                done.push(id);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            done
        }));
    }

    // Часть записей — на первом primary; затем он становится репликой (только чтение, сеансы оборваны).
    while ids(&url, &a).await.len() < 40 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    admin
        .execute(AssertSqlSafe(format!(
            "ALTER DATABASE {a} SET default_transaction_read_only = on"
        )))
        .await
        .expect("read-only");
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND application_name = $2",
    )
    .bind(&a)
    .bind(APP)
    .execute(&admin)
    .await
    .expect("terminate");

    let mut written = Vec::new();
    for w in writers {
        written.extend(w.await.expect("writer"));
    }
    assert_eq!(written.len(), 200);

    let (in_a, in_b) = (ids(&url, &a).await, ids(&url, &b).await);
    assert!(!in_a.is_empty(), "до переключения писали в первый primary");
    assert!(!in_b.is_empty(), "после переключения — во второй");
    let lost: Vec<_> = written
        .iter()
        .filter(|id| !in_a.contains(id) && !in_b.contains(id))
        .collect();
    assert!(lost.is_empty(), "потеряны записи: {lost:?}");

    let m = fo.metrics();
    assert_eq!(m.primary, 1, "{m:?}");
    assert_eq!(m.switches, 1, "{m:?}");
    assert!(m.retries >= 1, "{m:?}");
    assert!(m.last_switch.is_some() && !m.switching, "{m:?}");
    assert_eq!(m.gave_up, 0, "{m:?}");

    fo.pool().close().await;
    for db in [&a, &b] {
        admin
            .execute(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
            .await
            .expect("drop database");
    }
}
