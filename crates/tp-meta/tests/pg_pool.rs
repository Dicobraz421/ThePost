//! Пул из конфигурации, транзакции организации, проверки doctor (META-07); TLS канала (META-14).
//! Интеграционные тесты — в профиле dev (`common::dev_pool_config`); пароль — из хранилища секретов (META-17).

mod common;

use std::sync::Arc;

use tp_contracts::secrets::SecretRef;
use tp_contracts::tls::{InternalTls, Profile};
use tp_contracts::{Error, TenantId};
use tp_meta::doctor::{self, DoctorConfig, Status};
use tp_meta::pool::{self, NoAcquireObserver, PoolConfig};
use tp_meta::schema::db_err;

fn cfg(url: String, pgbouncer: bool) -> PoolConfig {
    PoolConfig {
        pgbouncer,
        max_connections: 4,
        ..common::dev_pool_config(&url)
    }
}

#[test]
fn config_validation_hides_password() {
    let empty = PoolConfig::default();
    assert!(matches!(
        empty.connect_options(&empty.url, false),
        Err(Error::Invalid(_))
    ));
    // Адрес с паролем — без разбора `dev_pool_config` (он переносит пароль в общее хранилище тестов).
    let dev = |url: &str| PoolConfig {
        url: url.into(),
        tls: InternalTls {
            allow_plaintext: true,
            ..InternalTls::default()
        },
        profile: Profile::Dev,
        ..PoolConfig::default()
    };
    let bad = dev("postgres://u:secret@h:port/db?sslmode=disable");
    match bad.connect_options(&bad.url, false) {
        Err(Error::Invalid(m)) => assert!(!m.contains("secret")),
        other => panic!("{other:?}"),
    }
    let good = dev("postgres://u:secret@h/db?sslmode=disable");
    assert!(!format!("{good:?}").contains("secret"));
    // Пароль в адресе — только dev; вне dev — ошибка без значения в тексте.
    assert!(good.check().is_ok());
    let prod = PoolConfig {
        url: "postgres://u:hunter2@h/db".into(),
        ..PoolConfig::default()
    };
    match prod.check() {
        Err(Error::Invalid(m)) => assert!(!m.contains("hunter2") && m.contains("password"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn password_from_secret_store() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let (_, password) = common::split_password(&url);
    let Some(password) = password else {
        eprintln!("в TEST_DATABASE_URL нет пароля — проверка пароля из хранилища пропущена");
        return;
    };
    let secret: SecretRef = format!("test/pool-{}#password", uuid::Uuid::now_v7().simple())
        .parse()
        .expect("ссылка");
    let store = common::secrets();
    store.set(&secret, &password);
    let c = PoolConfig {
        password: Some(secret.clone()),
        max_connections: 1,
        ..common::dev_pool_config(&url)
    };
    assert!(!pool::url_has_password(&c.url), "адрес без пароля");
    let pool = pool::connect(&c, &*store, Arc::new(NoAcquireObserver))
        .await
        .expect("connect");
    let one: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&mut *common::conn(&pool).await)
        .await
        .expect("select");
    assert_eq!(one, 1);

    // Отозванный секрет — понятная ошибка Unavailable, без значения.
    store.revoke(&secret);
    assert_eq!(
        pool::connect(&c, &*store, Arc::new(NoAcquireObserver))
            .await
            .err(),
        Some(Error::UNAVAILABLE)
    );
    assert_eq!(
        pool::connect_direct(&c, &*store, Arc::new(NoAcquireObserver))
            .await
            .err(),
        Some(Error::UNAVAILABLE)
    );
}

#[tokio::test]
async fn pool_modes_and_with_tenant() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let base = common::pool().await.expect("pool");
    let tenant = common::tenant(&base).await;
    for pgbouncer in [false, true] {
        let c = cfg(url.clone(), pgbouncer);
        let secrets = common::secrets();
        let pool = pool::connect(&c, &*secrets, Arc::new(NoAcquireObserver))
            .await
            .expect("connect");
        let password = c.password(&*secrets).await.expect("password");
        let lazy = pool::connect_lazy(
            &c,
            &c.url,
            password.as_ref(),
            None,
            Arc::new(NoAcquireObserver),
        )
        .expect("lazy");
        let direct = pool::connect_direct(&c, &*secrets, Arc::new(NoAcquireObserver))
            .await
            .expect("direct");
        for p in [&pool, &lazy, &direct] {
            let seen: String = pool::with_tenant(p, tenant, |tx| {
                Box::pin(async move {
                    sqlx::query_scalar("SELECT current_setting('app.tenant_id')")
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(db_err)
                })
            })
            .await
            .expect("with_tenant");
            assert_eq!(seen, tenant.to_string());
        }

        // SET LOCAL не переживает транзакцию: следующая видит пустой контекст.
        let after: Option<String> =
            sqlx::query_scalar("SELECT nullif(current_setting('app.tenant_id', true), '')")
                .fetch_one(&mut *common::conn(&pool).await)
                .await
                .expect("after");
        assert_eq!(after, None);
    }

    // Ошибка тела — откат.
    let pool = pool::connect(
        &cfg(url.clone(), false),
        &*common::secrets(),
        Arc::new(NoAcquireObserver),
    )
    .await
    .expect("connect");
    let other = TenantId::new();
    let res: tp_contracts::Result<()> = pool::with_tenant(&pool, other, |tx| {
        Box::pin(async move {
            sqlx::query("INSERT INTO tenants (tenant_id, name) VALUES ($1, 'rollback')")
                .bind(other.as_uuid())
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            Err(Error::Conflict)
        })
    })
    .await;
    assert_eq!(res, Err(Error::Conflict));
    let found: i64 = pool::with_system(&pool, |tx| {
        Box::pin(async move {
            sqlx::query_scalar("SELECT count(*) FROM tenants WHERE tenant_id = $1")
                .bind(other.as_uuid())
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)
        })
    })
    .await
    .expect("count");
    assert_eq!(found, 0);
}

#[tokio::test]
async fn doctor_structured_report() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let report = doctor::check(&pool, &DoctorConfig::default())
        .await
        .expect("doctor");
    let ids: Vec<_> = report.checks.iter().map(|c| c.id).collect();
    assert_eq!(
        ids,
        [
            "server_version",
            "sync_replication",
            "max_connections",
            "timezone",
            "log_timezone",
            "rls",
            "node_role",
            "tls"
        ]
    );
    assert_eq!(
        report.get("server_version").expect("get").status,
        Status::Ok
    );
    assert_eq!(report.get("timezone").expect("get").status, Status::Ok);
    assert_eq!(
        report.get("sync_replication").expect("get").status,
        Status::Skipped
    );
    assert!(
        report
            .checks
            .iter()
            .all(|c| !c.value.is_empty() && !c.message.is_empty())
    );

    // Enterprise без синхронных реплик (тестовая СУБД одиночная) — не соответствует.
    let enterprise = DoctorConfig {
        sync_replication: true,
        ..DoctorConfig::default()
    };
    let report = doctor::check(&pool, &enterprise).await.expect("doctor");
    assert_eq!(
        report.get("sync_replication").expect("get").status,
        Status::Fail
    );
    assert_eq!(report.status(), Status::Fail);
}

#[tokio::test]
async fn sslmode_disable_only_in_dev() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let dev = cfg(url, false);
    // Тот же адрес вне dev — ошибка конфигурации до подключения.
    let prod = PoolConfig {
        profile: tp_contracts::tls::Profile::Prod,
        ..dev.clone()
    };
    assert!(matches!(
        pool::connect(&prod, &*common::secrets(), Arc::new(NoAcquireObserver)).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        pool::connect_lazy(&prod, &dev.url, None, None, Arc::new(NoAcquireObserver)),
        Err(Error::Invalid(_))
    ));

    // doctor сообщает режим канала.
    let pool = pool::connect(&dev, &*common::secrets(), Arc::new(NoAcquireObserver))
        .await
        .expect("connect");
    let report = doctor::check(&pool, &DoctorConfig::default())
        .await
        .expect("doctor");
    let tls = report.get("tls").expect("tls");
    assert!(tls.value.starts_with("sslmode=disable"), "{tls:?}");
    assert_eq!(tls.status, Status::Warn);
}

/// META-55: пул из одного соединения, вторая транзакция ждёт — наблюдатель получает ожидание и его время;
/// отменённое ожидание не оставляет «ожидает».
#[tokio::test]
async fn acquire_observer_sees_wait() {
    use std::time::Duration;
    use tp_contracts::acquire::AcquireCounter;

    let Some(url) = common::pg_url() else {
        return;
    };
    let c = PoolConfig {
        max_connections: 1,
        acquire_timeout: Duration::from_secs(30),
        ..cfg(url, false)
    };
    // Ленивый пул: наблюдатель задан при создании и не видит пробного соединения `pool::connect`.
    let password = c.password(&*common::secrets()).await.expect("password");
    let counter = Arc::new(AcquireCounter::default());
    let pool =
        pool::connect_lazy(&c, &c.url, password.as_ref(), None, counter.clone()).expect("lazy");
    assert_eq!(pool.snapshot().max, 1);

    let waiting = |n: u64| {
        let counter = counter.clone();
        async move {
            for _ in 0..500 {
                if counter.waiting() == n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("ожидающих не {n}: {}", counter.waiting());
        }
    };
    let second = |pool: pool::DbPool| {
        tokio::spawn(async move {
            pool::with_system(&pool, |tx| {
                Box::pin(async move {
                    sqlx::query_scalar::<_, i32>("SELECT 1")
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(db_err)
                })
            })
            .await
        })
    };

    // Первая транзакция держит единственное соединение.
    let held = tp_meta::schema::begin(&pool, TenantId::DEFAULT)
        .await
        .expect("begin");
    assert_eq!((counter.waiting(), counter.acquired()), (0, 1));
    let snap = pool.snapshot();
    assert_eq!((snap.size, snap.idle, snap.in_use()), (1, 0, 1));

    // Вторая ждёт: наблюдатель видит ожидание, после освобождения — его время.
    let task = second(pool.clone());
    waiting(1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let before = counter.waited();
    drop(held);
    assert_eq!(task.await.expect("join"), Ok(1));
    assert_eq!(
        (counter.waiting(), counter.acquired(), counter.failed()),
        (0, 2, 0)
    );
    assert!(counter.waited() - before >= Duration::from_millis(50));

    // Отменённое ожидание не оставляет «ожидает» и не считается отказом.
    let held = tp_meta::schema::begin_system(&pool).await.expect("begin");
    let task = second(pool.clone());
    waiting(1).await;
    task.abort();
    assert!(task.await.expect_err("отменена").is_cancelled());
    waiting(0).await;
    assert_eq!(
        (counter.acquired(), counter.failed(), counter.cancelled()),
        (3, 0, 1)
    );
    drop(held);

    // Тайм-аут пула — исход «отказ».
    let short = PoolConfig {
        acquire_timeout: Duration::from_millis(200),
        ..c
    };
    let timed = Arc::new(AcquireCounter::default());
    let pool = pool::connect_lazy(&short, &short.url, password.as_ref(), None, timed.clone())
        .expect("lazy");
    let held = tp_meta::schema::begin_system(&pool).await.expect("begin");
    let r = pool::with_system(&pool, |_| Box::pin(async { Ok(()) })).await;
    assert!(matches!(r, Err(Error::Unavailable { .. })), "{r:?}");
    assert_eq!((timed.waiting(), timed.failed()), (0, 1));
    drop(held);
}
