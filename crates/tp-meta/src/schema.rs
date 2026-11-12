//! Схема БД: миграции ядра, транзакции в контексте организации (RLS), ошибки sqlx (SRS 3.1, 3.6, 3.8).
//!
//! Миграции встроены в бинарный файл (`migrations/*.sql`) и применяются `sqlx::migrate::Migrator`
//! под рекомендательной блокировкой: параллельный старт узлов безопасен, повторное применение — пустая операция.

use std::borrow::Cow;

use sqlx::migrate::{MigrateError, Migration, MigrationType, Migrator};
use sqlx::{AssertSqlSafe, Postgres, SqlSafeStr, Transaction};
use tp_contracts::{Error, Result, TenantId};

use crate::pool::DbPool;

/// Миграции ядра: (версия, описание, SQL). Версии только добавляются; применённые не меняются.
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "schema", include_str!("../migrations/0001_schema.sql")),
    (2, "nodes", include_str!("../migrations/0002_nodes.sql")),
    (
        3,
        "shard credentials",
        include_str!("../migrations/0003_shard_credentials.sql"),
    ),
    (
        4,
        "blob damaged",
        include_str!("../migrations/0004_blob_damaged.sql"),
    ),
    (5, "queue", include_str!("../migrations/0005_queue.sql")),
    (
        6,
        "retention",
        include_str!("../migrations/0006_retention.sql"),
    ),
    (7, "acl", include_str!("../migrations/0007_acl.sql")),
    (
        8,
        "index manifests",
        include_str!("../migrations/0008_index_manifests.sql"),
    ),
    (
        9,
        "app passwords",
        include_str!("../migrations/0009_app_passwords.sql"),
    ),
    (
        10,
        "distribution groups",
        include_str!("../migrations/0010_distribution_groups.sql"),
    ),
    (
        11,
        "delivery and audit logs",
        include_str!("../migrations/0011_delivery_audit.sql"),
    ),
    (
        12,
        "node roles",
        include_str!("../migrations/0012_node_roles.sql"),
    ),
    (
        13,
        "blob compression",
        include_str!("../migrations/0013_blob_compression.sql"),
    ),
    (
        14,
        "trash purge",
        include_str!("../migrations/0014_trash_purge.sql"),
    ),
    (
        15,
        "node info and abuse blocks",
        include_str!("../migrations/0015_node_info_abuse_blocks.sql"),
    ),
    (
        16,
        "transport",
        include_str!("../migrations/0016_transport.sql"),
    ),
    (
        17,
        "transport rewrites",
        include_str!("../migrations/0017_transport_rewrites.sql"),
    ),
    (
        18,
        "local admin tokens",
        include_str!("../migrations/0018_local_admin_tokens.sql"),
    ),
    (
        19,
        "accounts login c",
        include_str!("../migrations/0019_accounts_login_c.sql"),
    ),
    (
        20,
        "mailbox shards",
        include_str!("../migrations/0020_mailbox_shards.sql"),
    ),
    (21, "dav", include_str!("../migrations/0021_dav.sql")),
    (22, "sieve", include_str!("../migrations/0022_sieve.sql")),
    (
        23,
        "vacation log",
        include_str!("../migrations/0023_vacation_log.sql"),
    ),
    (
        24,
        "schedule",
        include_str!("../migrations/0024_schedule.sql"),
    ),
    (
        25,
        "busy recurring",
        include_str!("../migrations/0025_busy_recurring.sql"),
    ),
    (
        26,
        "rcpts per hour",
        include_str!("../migrations/0026_rcpts_per_hour.sql"),
    ),
    (
        27,
        "submissions mailbox audit",
        include_str!("../migrations/0027_submissions_mailbox_audit.sql"),
    ),
    (
        28,
        "audit export",
        include_str!("../migrations/0028_audit_export.sql"),
    ),
    (
        29,
        "address book",
        include_str!("../migrations/0029_address_book.sql"),
    ),
    (
        30,
        "contracts 38",
        include_str!("../migrations/0030_contracts_38.sql"),
    ),
    (
        31,
        "account kind",
        include_str!("../migrations/0031_account_kind.sql"),
    ),
    (
        32,
        "sync holds",
        include_str!("../migrations/0032_sync_holds.sql"),
    ),
];

/// Мигратор схемы ядра.
pub fn migrator() -> Migrator {
    migrator_until(i64::MAX)
}

/// Мигратор схемы ядра до версии `version` включительно — проверка миграции на данных прежней схемы.
pub fn migrator_until(version: i64) -> Migrator {
    Migrator::with_migrations(
        MIGRATIONS
            .iter()
            .filter(|(v, _, _)| *v <= version)
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
/// Соединение — из пула `pool` через его наблюдателя.
pub async fn migrate(pool: &DbPool) -> Result<()> {
    let mut conn = pool.acquire().await?;
    migrator()
        .run_direct(None, &mut *conn, false)
        .await
        .map_err(|e| match e {
            MigrateError::Execute(e) | MigrateError::ExecuteMigration(e, _) => db_err(e),
            e => {
                tracing::error!(error = %e, "миграция схемы");
                Error::internal(e.to_string())
            }
        })
}

/// Транзакция в контексте организации: `app.tenant_id` (SET LOCAL) — RLS видит только её строки.
pub async fn begin(pool: &DbPool, tenant: TenantId) -> Result<Transaction<'static, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    Ok(tx)
}

/// Системная транзакция по всем организациям (GC BLOB, журнал событий): `app.all_tenants` (SET LOCAL).
pub async fn begin_system(pool: &DbPool) -> Result<Transaction<'static, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.all_tenants', 'on', true)")
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    Ok(tx)
}

/// Ошибка sqlx → ошибка границы крейта. Обрыв, `25006` (сервер только для чтения — бывший primary) и
/// `57P01` (остановка сервера) — `Unavailable`: признаки переключения СУБД ([`crate::failover`]).
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
                        "25006"
                            | "40001"
                            | "40P01"
                            | "53300"
                            | "55P03"
                            | "57P01"
                            | "57P02"
                            | "57P03"
                    ) =>
            {
                Error::UNAVAILABLE
            }
            code => {
                tracing::error!(code, error = %e, "ошибка СУБД");
                Error::internal(e.to_string())
            }
        },
        sqlx::Error::Tls(t) => tls_err(t.as_ref()),
        sqlx::Error::Io(_)
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

/// Ошибка TLS канала до СУБД (META-56): причина — в журнал (значения ключа в ней нет). Ключ клиентского
/// сертификата не подходит к сертификату — `Invalid` с понятным текстом: rustls сверяет открытый ключ сертификата
/// с закрытым при сборке параметров TLS (`CertifiedKey::from_der`, текст «keys may not be consistent:
/// KeyMismatch»), до обмена с сервером. Остальное — `Unavailable` (повтор после исправления поможет).
fn tls_err(e: &(dyn std::error::Error + Send + Sync)) -> Error {
    let text = e.to_string();
    if text.contains("KeyMismatch") {
        tracing::error!(error = %text, "СУБД: ключ клиентского сертификата не подходит к сертификату");
        return Error::invalid(
            "database.tls: ключ клиентского сертификата (tls.key или tls.key_file) не подходит к сертификату \
             tls.cert_file",
        );
    }
    tracing::warn!(error = %text, "СУБД: ошибка TLS канала");
    Error::UNAVAILABLE
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

#[cfg(test)]
mod tests {
    use super::MIGRATIONS;

    /// Версии в списке идут подряд с 1; каждому номеру соответствует ровно один файл
    /// `migrations/NNNN_*.sql` с тем же содержимым; лишних файлов в каталоге нет.
    #[test]
    fn migration_numbers_are_sequential_and_match_files() {
        for (i, (version, _, _)) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                *version,
                i64::try_from(i).expect("индекс") + 1,
                "версии миграций должны идти подряд с 1 (позиция {i})"
            );
        }

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut files: Vec<(i64, String)> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("каталог migrations") {
            let name = entry
                .expect("запись каталога")
                .file_name()
                .to_string_lossy()
                .into_owned();
            let (num, rest) = name.split_once('_').unwrap_or((&name, ""));
            let num: i64 = num
                .parse()
                .unwrap_or_else(|_| panic!("имя файла миграции без номера: {name}"));
            assert!(
                num.to_string().len() <= 4 && name.len() > 5 && rest.ends_with(".sql"),
                "имя файла миграции не вида NNNN_*.sql: {name}"
            );
            files.push((num, name));
        }
        files.sort();

        for pair in files.windows(2) {
            assert_ne!(
                pair[0].0, pair[1].0,
                "два файла с одним номером миграции: {} и {}",
                pair[0].1, pair[1].1
            );
        }
        let nums: Vec<i64> = files.iter().map(|(n, _)| *n).collect();
        let versions: Vec<i64> = MIGRATIONS.iter().map(|(v, _, _)| *v).collect();
        assert_eq!(
            nums, versions,
            "файлы в migrations/ и список MIGRATIONS расходятся"
        );

        for ((version, _, sql), (_, name)) in MIGRATIONS.iter().zip(&files) {
            assert!(
                name.starts_with(&format!("{version:04}_")),
                "номер в имени {name} не совпадает с версией {version}"
            );
            let on_disk = std::fs::read_to_string(dir.join(name)).expect("файл миграции");
            assert_eq!(&on_disk, sql, "файл {name} не тот, что подключён в списке");
        }
    }
}
