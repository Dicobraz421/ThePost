//! META-41: слушатель сигналов и миграции при списке прямых адресов СУБД (PgBouncer, эталон Enterprise-ВМ).

mod common;

use std::sync::Arc;

use tp_meta::pool::{NoAcquireObserver, PoolConfig, SchemaOwner, connect_direct};
use tp_meta::schema;
use tp_meta::signals::connect_listener;

/// Адрес, на котором PostgreSQL нет (соединение отклоняется сразу).
const DEAD: &str = "postgres://postgres@127.0.0.1:1/postgres?sslmode=disable";

/// Секция пула за «PgBouncer» (адрес узла — сама тестовая БД) с прямыми адресами: первый недоступен.
fn behind_pgbouncer(url: &str) -> PoolConfig {
    let cfg = common::dev_pool_config(url);
    let direct = cfg.url.clone();
    PoolConfig {
        pgbouncer: true,
        direct_url: Some(DEAD.to_owned()),
        direct_urls: vec![direct],
        ..cfg
    }
}

async fn port_of(pool: &tp_meta::pool::DbPool) -> i32 {
    sqlx::query_scalar("SELECT current_setting('port')::int")
        .fetch_one(&mut *common::conn(pool).await)
        .await
        .expect("port")
}

#[tokio::test]
async fn listener_searches_primary_in_direct_urls() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let cfg = behind_pgbouncer(&url);
    let listener = connect_listener(&cfg, common::secrets(), Arc::new(NoAcquireObserver))
        .await
        .expect("listener");
    assert_eq!(listener.metrics().primary, 1, "второй прямой адрес");
    assert!(port_of(listener.pool()).await > 1);

    // Без прямых адресов за PgBouncer — ошибка конфигурации.
    let none = PoolConfig {
        direct_url: None,
        direct_urls: Vec::new(),
        ..cfg
    };
    assert!(matches!(
        connect_listener(&none, common::secrets(), Arc::new(NoAcquireObserver)).await,
        Err(tp_contracts::Error::Invalid(_))
    ));
}

#[tokio::test]
async fn migrations_search_primary_in_direct_urls() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let cfg = behind_pgbouncer(&url);
    // С учётными данными узла.
    let pool = connect_direct(
        &cfg,
        common::secrets().as_ref(),
        Arc::new(NoAcquireObserver),
    )
    .await
    .expect("direct");
    schema::migrate(&pool).await.expect("migrate");
    assert!(port_of(&pool).await > 1);
    pool.close().await;

    // От владельца схемы (`database.owner`): имя роли — из адреса тестовой БД, пароль — тот же секрет.
    let (plain, _) = common::split_password(&url);
    let user = <sqlx::postgres::PgConnectOptions as std::str::FromStr>::from_str(&plain)
        .expect("options")
        .get_username()
        .to_owned();
    let owner = PoolConfig {
        owner: Some(SchemaOwner {
            user,
            password: cfg.password.clone(),
        }),
        ..cfg.clone()
    };
    let pool = connect_direct(
        &owner,
        common::secrets().as_ref(),
        Arc::new(NoAcquireObserver),
    )
    .await
    .expect("owner");
    schema::migrate(&pool).await.expect("migrate");
    pool.close().await;

    // Ни один адрес не доступен — Unavailable.
    let dead = PoolConfig {
        direct_urls: vec![DEAD.to_owned()],
        ..cfg
    };
    assert_eq!(
        connect_direct(
            &dead,
            common::secrets().as_ref(),
            Arc::new(NoAcquireObserver)
        )
        .await
        .err(),
        Some(tp_contracts::Error::UNAVAILABLE)
    );
}
