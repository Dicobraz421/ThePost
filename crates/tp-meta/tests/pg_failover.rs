//! Поиск primary и переключение СУБД (META-25): две БД одного сервера — два «экземпляра», перевод primary
//! в режим только для чтения с обрывом сеансов (как при перезапуске бывшего primary репликой) или пропавший
//! адрес (прокси TCP, META-38).

mod common;

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Executor, PgPool};
use tokio::net::{TcpListener, TcpStream};
use tp_contracts::lease::LeaseStore;
use tp_contracts::meta::AccountStore;
use tp_contracts::{Error, TenantId};
use tp_meta::failover::{FailoverConfig, connect_failover};
use tp_meta::schema::db_err;
use tp_meta::{MetaConfig, PgMeta};
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

/// Прокси TCP к серверу тестовой БД: «адрес primary», который можно отключить (узел пропал).
struct Proxy {
    addr: std::net::SocketAddr,
    tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
}

impl Proxy {
    async fn start(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::default();
        let conns = tasks.clone();
        let accept = tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let upstream = upstream.clone();
                let conn = tokio::spawn(async move {
                    if let Ok(mut server) = TcpStream::connect(&upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    }
                });
                conns.lock().expect("lock").push(conn.abort_handle());
            }
        });
        tasks.lock().expect("lock").push(accept.abort_handle());
        Self { addr, tasks }
    }

    /// Адрес больше не принимает соединения, открытые — оборваны.
    fn kill(&self) {
        for t in self.tasks.lock().expect("lock").drain(..) {
            t.abort();
        }
    }
}

/// `url` с хостом и портом `addr`.
fn via(url: &str, addr: std::net::SocketAddr) -> String {
    let (scheme, rest) = url.split_once("://").expect("scheme");
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let user = authority.rsplit_once('@').map(|(u, _)| format!("{u}@"));
    format!("{scheme}://{}{addr}{tail}", user.unwrap_or_default())
}

async fn migrated(url: &str, db: &str) -> PgPool {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url_for(url, db))
        .await
        .expect("connect");
    tp_meta::schema::migrate(&pool).await.expect("миграции");
    pool
}

/// META-38: `PgMeta` над `Failover` — адрес primary пропал между вызовами: чтение повторено на новом
/// primary и успешно; запись без ключа при отказе не повторяется (`Unavailable` с `retry_after`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn meta_read_retried_on_new_primary() {
    const APP_META: &str = "tp_failover_meta_test";
    let Some(url) = common::pg_url() else {
        return;
    };
    let opts = PgConnectOptions::from_str(&url).expect("url");
    if opts.get_host().starts_with('/') {
        eprintln!("TEST_DATABASE_URL — сокет Unix: прокси TCP невозможен, тест пропущен");
        return;
    }
    let upstream = format!("{}:{}", opts.get_host(), opts.get_port());
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("подключение к TEST_DATABASE_URL");
    let tag = Uuid::now_v7().simple().to_string();
    let (a, b) = (format!("tp_fom_a_{tag}"), format!("tp_fom_b_{tag}"));
    create_db(&admin, &url, &a).await;
    create_db(&admin, &url, &b).await;
    migrated(&url, &a).await.close().await;
    // Учётная запись есть только во втором «экземпляре»: её чтение видно лишь после переключения.
    let meta_b = PgMeta::new(migrated(&url, &b).await, MetaConfig::default());
    let scope = common::account(&meta_b).await;
    meta_b.pool().close().await;

    let proxy = Proxy::start(upstream).await;
    let mut cfg = common::dev_pool_config(&via(&url_for(&url, &a), proxy.addr));
    cfg.urls = vec![common::dev_url(
        &common::split_password(&url_for(&url, &b)).0,
    )];
    cfg.application_name = APP_META.into();
    cfg.acquire_timeout = Duration::from_secs(2);
    cfg.failover = FailoverConfig {
        retry_initial: Duration::from_millis(50),
        retry_max: Duration::from_millis(500),
        retry_for: Duration::from_secs(30),
        probe_timeout: Duration::from_secs(2),
    };
    let fo = connect_failover(&cfg, common::secrets())
        .await
        .expect("connect_failover");
    let meta = PgMeta::from_failover(&fo, MetaConfig::default());
    assert_eq!(fo.metrics().primary, 0);
    let before = meta
        .get(scope.tenant, scope.account)
        .await
        .expect("чтение на первом primary");
    assert!(before.is_none(), "в первом экземпляре учётной записи нет");

    // Первый primary пропал: соединения оборваны, адрес не отвечает.
    proxy.kill();
    let after = meta
        .get(scope.tenant, scope.account)
        .await
        .expect("чтение повторено на новом primary");
    assert_eq!(after.map(|acc| acc.id), Some(scope.account));
    let m = fo.metrics();
    assert_eq!((m.primary, m.switches, m.gave_up), (1, 1, 0), "{m:?}");
    assert!(m.retries >= 1, "{m:?}");

    // Новый primary стал репликой, другого нет: запись без ключа — один раз, временный отказ с паузой.
    admin
        .execute(AssertSqlSafe(format!(
            "ALTER DATABASE {b} SET default_transaction_read_only = on"
        )))
        .await
        .expect("read-only");
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND application_name = $2",
    )
    .bind(&b)
    .bind(APP_META)
    .execute(&admin)
    .await
    .expect("terminate");
    let retries = fo.metrics().retries;
    let err = meta
        .try_acquire("meta38", "node-1", Duration::from_secs(30))
        .await
        .expect_err("запись на реплике");
    assert_eq!(
        err,
        Error::Unavailable {
            retry_after: Some(Duration::from_millis(500))
        }
    );
    assert_eq!(fo.metrics().retries, retries, "запись не повторялась");

    fo.pool().close().await;
    for db in [&a, &b] {
        admin
            .execute(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
            .await
            .expect("drop database");
    }
}
