//! META-18: динамические учётные данные PostgreSQL — ротация без обрыва текущих запросов.
//! Роли создаёт тест (нужно право CREATEROLE у роли `TEST_DATABASE_URL`); выдачу database secrets engine
//! заменяет `MemSecretStore` с коротким сроком аренды.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::{Executor, PgPool};
use tp_contracts::secrets::SecretRef;
use tp_meta::credentials::DynamicCredentials;
use tp_meta::pool::{self, PoolConfig};
use tp_testkit::MemSecretStore;

async fn current_user(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(pool)
        .await
        .expect("current_user")
}

#[tokio::test]
async fn new_connections_use_rotated_credentials_running_queries_survive() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let admin = common::pool().await.expect("pool");
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let roles: Vec<(String, String)> = (1..=2)
        .map(|i| (format!("tp_dyn_{tag}_{i}"), format!("pw-{tag}-{i}")))
        .collect();
    for (role, password) in &roles {
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "CREATE ROLE {role} LOGIN PASSWORD '{password}'"
            )))
            .await
            .expect("роль");
    }

    let r = |s: String| s.parse::<SecretRef>().expect("ссылка");
    let dc = DynamicCredentials {
        username: r(format!("database/creds/{tag}#username")),
        password: r(format!("database/creds/{tag}#password")),
        renew_at_percent: 50,
        retry_interval: Duration::from_millis(100),
    };
    let store = MemSecretStore::default();
    let issue = |(role, password): &(String, String)| {
        // Непродлеваемая аренда: на половине срока — новая выдача.
        let ttl = Duration::from_secs(2);
        store.set_dynamic(&dc.username, role, ttl, false);
        store.set_dynamic(&dc.password, password, ttl, false);
    };
    issue(&roles[0]);
    let (base, _) = common::split_password(&url);
    let cfg = PoolConfig {
        password: None,
        dynamic_credentials: Some(dc.clone()),
        max_connections: 4,
        ..common::dev_pool_config(&base)
    };
    let pool = pool::connect_rotating(&cfg, Arc::new(store.clone()))
        .await
        .expect("пул");
    assert_eq!(current_user(&pool).await, roles[0].0);

    // Текущая транзакция со старыми учётными данными переживает ротацию.
    let mut tx = pool.begin().await.expect("begin");
    let before: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&mut *tx)
        .await
        .expect("до ротации");
    assert_eq!(before, roles[0].0);
    issue(&roles[1]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while pool.connect_options().get_username() != roles[1].0 {
        assert!(Instant::now() < deadline, "ротация не произошла");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Новое соединение (старое занято транзакцией) — с новыми учётными данными.
    assert_eq!(current_user(&pool).await, roles[1].0);
    let during: (String, String) = sqlx::query_as("SELECT current_user::text, pg_sleep(0.2)::text")
        .fetch_one(&mut *tx)
        .await
        .expect("после ротации");
    assert_eq!(during.0, roles[0].0);
    tx.commit().await.expect("commit");
    // Соединение со старыми учётными данными вернулось в пул и закрывается при следующей выдаче.
    for _ in 0..4 {
        assert_eq!(current_user(&pool).await, roles[1].0);
    }
    pool.close().await;

    for (role, _) in &roles {
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = '{role}'"
            )))
            .await
            .expect("terminate");
        admin
            .execute(sqlx::AssertSqlSafe(format!("DROP ROLE IF EXISTS {role}")))
            .await
            .expect("drop role");
    }
}
