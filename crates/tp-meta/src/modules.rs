//! Миграции модулей: схема `mod_<имя>` с собственной таблицей версий
//! (CONTRACTS.md#Extension points; SRS 2.5, 6.6).
//!
//! Включение модуля — [`migrate_module`] перед `Module::register`: схема ядра не меняется, объекты модуля
//! создаются в его схеме (`search_path` = `mod_<имя>, public`). Отключение — модуль не регистрируется,
//! данные остаются; удаление — [`drop_module`] (`DROP SCHEMA … CASCADE`).
//!
//! Все недостающие миграции применяются одной транзакцией под рекомендательной блокировкой модуля:
//! параллельный старт узлов безопасен, ошибка откатывает всё. Поэтому SQL миграций не может содержать
//! операторы вне транзакции (`CREATE INDEX CONCURRENTLY`). Версии в БД новее известных узлу допустимы
//! (последовательное обновление, N−1); изменённая применённая миграция или пропущенная версия
//! ниже применённых — `Conflict`.

use std::collections::HashMap;

use sqlx::{AssertSqlSafe, FromRow, PgConnection, PgPool};
use tp_contracts::ext::{Migration, Module, validate_module_name};
use tp_contracts::{Error, Result};

use crate::schema::{db_err, to_u32};

/// Таблица версий в схеме модуля.
const VERSIONS: &str = "tp_migrations";

#[derive(FromRow)]
struct Applied {
    version: i32,
    same: bool,
}

/// Схема модуля по имени: `mod_<имя>`.
pub fn schema_of(name: &str) -> Result<String> {
    validate_module_name(name)?;
    Ok(format!("mod_{name}"))
}

fn check_module(module: &dyn Module) -> Result<String> {
    let schema = schema_of(module.name())?;
    if module.schema() != schema {
        return Err(Error::invalid(format!(
            "схема модуля {} должна быть {schema}",
            module.name()
        )));
    }
    let mut prev = 0;
    for m in module.migrations() {
        if m.version <= prev {
            return Err(Error::invalid(format!(
                "миграции модуля {}: версии должны строго возрастать с 1 ({} после {prev})",
                module.name(),
                m.version
            )));
        }
        prev = m.version;
    }
    Ok(schema)
}

fn version_arg(v: u32) -> Result<i32> {
    i32::try_from(v).map_err(|_| Error::invalid(format!("версия миграции вне integer: {v}")))
}

/// SQL миграции без зависимости от окончаний строк рабочей копии.
fn sql_of(m: &Migration) -> String {
    m.sql.replace("\r\n", "\n")
}

async fn lock(tx: &mut PgConnection, schema: &str) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('thepost:' || $1, 0))")
        .bind(schema)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Применяет недостающие миграции модуля; возвращает число применённых.
pub async fn migrate_module(pool: &PgPool, module: &dyn Module) -> Result<usize> {
    let schema = check_module(module)?;
    let mut tx = pool.begin().await.map_err(db_err)?;
    lock(&mut tx, &schema).await?;
    // Имя схемы проверено `validate_module_name`: только [a-z0-9_].
    sqlx::query(AssertSqlSafe(format!(
        "CREATE SCHEMA IF NOT EXISTS {schema}"
    )))
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    sqlx::query(AssertSqlSafe(format!(
        "CREATE TABLE IF NOT EXISTS {schema}.{VERSIONS} (
             version    integer PRIMARY KEY,
             name       text NOT NULL,
             checksum   text NOT NULL,
             applied_at timestamptz NOT NULL DEFAULT now())"
    )))
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;

    let known = module.migrations();
    let versions = known
        .iter()
        .map(|m| version_arg(m.version))
        .collect::<Result<Vec<_>>>()?;
    let sqls: Vec<String> = known.iter().map(sql_of).collect();
    // Контрольная сумма — md5 СУБД: сравнение применённых миграций с известными узлу.
    let applied: Vec<Applied> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT a.version, k.sql IS NULL OR a.checksum = md5(k.sql) AS same
         FROM {schema}.{VERSIONS} a
         LEFT JOIN unnest($1::int4[], $2::text[]) AS k(version, sql) USING (version)
         ORDER BY a.version"
    )))
    .bind(&versions)
    .bind(&sqls)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;

    let done: HashMap<i32, bool> = applied.iter().map(|a| (a.version, a.same)).collect();
    if let Some(a) = applied.iter().find(|a| !a.same) {
        return Err(conflict(module, a.version, "применённая миграция изменена"));
    }
    let top = applied.last().map_or(0, |a| a.version);
    let pending: Vec<_> = known
        .iter()
        .zip(&versions)
        .zip(&sqls)
        .filter(|((_, v), _)| !done.contains_key(*v))
        .collect();
    if let Some(((_, v), _)) = pending.iter().find(|((_, v), _)| **v < top) {
        return Err(conflict(module, **v, "версия ниже уже применённых"));
    }

    sqlx::query("SELECT set_config('search_path', $1, true)")
        .bind(format!("{schema}, public"))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    for ((m, v), sql) in &pending {
        sqlx::raw_sql(AssertSqlSafe(sql.to_string()))
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                tracing::error!(
                    module = module.name(),
                    version = m.version,
                    "миграция модуля"
                );
                db_err(e)
            })?;
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {schema}.{VERSIONS} (version, name, checksum) VALUES ($1, $2, md5($3))"
        )))
        .bind(**v)
        .bind(m.name)
        .bind(sql.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    tx.commit().await.map_err(db_err)?;
    if !pending.is_empty() {
        tracing::info!(
            module = module.name(),
            applied = pending.len(),
            "миграции модуля применены"
        );
    }
    Ok(pending.len())
}

fn conflict(module: &dyn Module, version: i32, why: &str) -> Error {
    tracing::error!(module = module.name(), version, "{why}");
    Error::Conflict
}

/// Последняя применённая версия схемы модуля; `None` — схема не создана.
pub async fn applied_version(pool: &PgPool, name: &str) -> Result<Option<u32>> {
    let schema = schema_of(name)?;
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(format!("{schema}.{VERSIONS}"))
        .fetch_one(pool)
        .await
        .map_err(db_err)?;
    if !exists {
        return Ok(None);
    }
    let v: Option<i32> = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT max(version) FROM {schema}.{VERSIONS}"
    )))
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    v.map(|v| to_u32(i64::from(v))).transpose()
}

/// Удаляет схему модуля со всеми данными (удаление модуля). Схема ядра не меняется.
pub async fn drop_module(pool: &PgPool, name: &str) -> Result<()> {
    let schema = schema_of(name)?;
    let mut tx = pool.begin().await.map_err(db_err)?;
    lock(&mut tx, &schema).await?;
    sqlx::query(AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS {schema} CASCADE"
    )))
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)
}
