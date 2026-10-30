//! Схема БД: миграции ядра, транзакции в контексте организации (RLS), ошибки sqlx (SRS 3.1, 3.6, 3.8).
//!
//! Миграции встроены в бинарный файл (`migrations/*.sql`) и применяются `sqlx::migrate::Migrator`
//! под рекомендательной блокировкой: параллельный старт узлов безопасен, повторное применение — пустая операция.

use std::borrow::Cow;

use sqlx::migrate::{MigrateError, Migration, MigrationType, Migrator};
use sqlx::{AssertSqlSafe, PgPool, Postgres, SqlSafeStr, Transaction};
use tp_contracts::{Error, Result, TenantId};

/// Миграции ядра: (версия, описание, SQL). Версии только добавляются; применённые не меняются.
const MIGRATIONS: &[(i64, &str, &str)] =
    &[(1, "schema", include_str!("../migrations/0001_schema.sql"))];

/// Мигратор схемы ядра.
pub fn migrator() -> Migrator {
    Migrator::with_migrations(
        MIGRATIONS
            .iter()
            .map(|(version, desc, sql)| {
                Migration::new(
                    *version,
                    Cow::Borrowed(*desc),
                    MigrationType::Simple,
                    // Контрольная сумма не зависит от окончаний строк в рабочей копии.
                    AssertSqlSafe(sql.replace("\r\n", "\n")).into_sql_str(),
                    false,
                )
            })
            .collect(),
    )
}

/// Применяет недостающие миграции ядра.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    migrator().run(pool).await.map_err(|e| match e {
        MigrateError::Execute(e) | MigrateError::ExecuteMigration(e, _) => db_err(e),
        e => {
            tracing::error!(error = %e, "миграция схемы");
            Error::internal(e.to_string())
        }
    })
}

/// Транзакция в контексте организации: `app.tenant_id` (SET LOCAL) — RLS видит только её строки.
pub async fn begin(pool: &PgPool, tenant: TenantId) -> Result<Transaction<'static, Postgres>> {
    let mut tx = pool.begin().await.map_err(db_err)?;
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    Ok(tx)
}

/// Системная транзакция по всем организациям (GC BLOB, журнал событий): `app.all_tenants` (SET LOCAL).
pub async fn begin_system(pool: &PgPool) -> Result<Transaction<'static, Postgres>> {
    let mut tx = pool.begin().await.map_err(db_err)?;
    sqlx::query("SELECT set_config('app.all_tenants', 'on', true)")
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    Ok(tx)
}

/// Ошибка sqlx → ошибка границы крейта.
pub fn db_err(e: sqlx::Error) -> Error {
    match &e {
        sqlx::Error::RowNotFound => Error::NotFound,
        sqlx::Error::Database(d) => match d.code().as_deref() {
            Some("23505") => Error::Conflict,
            Some("23502" | "23503" | "23514" | "22003" | "22P02") => {
                Error::Invalid(d.message().to_owned())
            }
            Some("42501") => Error::Forbidden,
            Some(c)
                if c.starts_with("08")
                    || matches!(
                        c,
                        "40001" | "40P01" | "53300" | "55P03" | "57P01" | "57P02" | "57P03"
                    ) =>
            {
                Error::UNAVAILABLE
            }
            code => {
                tracing::error!(code, error = %e, "ошибка СУБД");
                Error::internal(e.to_string())
            }
        },
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed
        | sqlx::Error::BeginFailed => Error::UNAVAILABLE,
        _ => {
            tracing::error!(error = %e, "ошибка sqlx");
            Error::internal(e.to_string())
        }
    }
}

/// Значение столбца `bigint` → `u32` (UID, UIDVALIDITY, поколение).
pub(crate) fn to_u32(v: i64) -> Result<u32> {
    u32::try_from(v).map_err(|_| Error::internal(format!("значение вне u32: {v}")))
}

/// Значение столбца `bigint` → `u64` (размеры, ModSeq).
pub(crate) fn to_u64(v: i64) -> Result<u64> {
    u64::try_from(v).map_err(|_| Error::internal(format!("отрицательное значение: {v}")))
}

/// `u64` → `bigint`.
pub(crate) fn to_i64(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| Error::invalid(format!("значение вне bigint: {v}")))
}
