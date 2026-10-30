//! Пул из конфигурации, транзакции организации, проверки doctor (META-07).

mod common;

use tp_contracts::{Error, TenantId};
use tp_meta::doctor::{self, DoctorConfig, Status};
use tp_meta::pool::{self, PoolConfig};
use tp_meta::schema::db_err;

fn cfg(url: String, pgbouncer: bool) -> PoolConfig {
    PoolConfig {
        url,
        pgbouncer,
        max_connections: 4,
        ..PoolConfig::default()
    }
}

#[test]
fn config_validation_hides_password() {
    let empty = PoolConfig::default();
    assert!(matches!(
        empty.connect_options(&empty.url, false),
        Err(Error::Invalid(_))
    ));
    let bad = cfg("postgres://u:secret@h:port/db".into(), false);
    match bad.connect_options(&bad.url, false) {
        Err(Error::Invalid(m)) => assert!(!m.contains("secret")),
        other => panic!("{other:?}"),
    }
    let good = cfg("postgres://u:secret@h/db".into(), true);
    assert!(!format!("{good:?}").contains("secret"));
}

#[tokio::test]
async fn pool_modes_and_with_tenant() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let base = common::pool().await.expect("pool");
    let tenant = common::tenant(&base).await;
    for pgbouncer in [false, true] {
        let pool = pool::connect(&cfg(url.clone(), pgbouncer))
            .await
            .expect("connect");
        let lazy = pool::connect_lazy(&cfg(url.clone(), pgbouncer), &url).expect("lazy");
        let direct = pool::connect_direct(&cfg(url.clone(), pgbouncer))
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
                .fetch_one(&pool)
                .await
                .expect("after");
        assert_eq!(after, None);
    }

    // Ошибка тела — откат.
    let pool = pool::connect(&cfg(url.clone(), false))
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
            "rls"
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
