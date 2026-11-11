//! Скрипты Sieve и автоответ на PostgreSQL (CONTRACTS.md#Sieve; META-49, META-50).
//!
//! Скрипты (`sieve_scripts`) и автоответ (`vacation`) — таблицы ящика на шарде учётной записи; скрипт организации
//! (`sieve_org_scripts`) — в основной БД. Изменение блокирует строку `mailbox_state` и увеличивает её `sieve_state`
//! (JMAP `state`): лимит числа скриптов и «один активный» не гоняются с параллельной записью.
//!
//! Журнал автоответов (`vacation_log`, [`VacationLog`]) — тоже на шарде учётной записи; записи старше
//! `meta.sieve.vacation_log_ttl` удаляет [`PgMeta::purge_vacation_log`] (роль maintenance).

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection};
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::meta::AccountScope;
use tp_contracts::sieve::{
    ActiveScript, ScriptInfo, SieveLimits, SieveStore, Vacation, VacationLog, sieve_changed,
    valid_script_name,
};
use tp_contracts::{Error, Result, TenantId};

use crate::PgMeta;
use crate::mailboxes::ensure_state;
use crate::pool::with_system;
use crate::schema::{begin, db_err, to_u64};
use crate::shards::ShardId;

/// Секция `meta.sieve`: лимиты хранилища скриптов ([`SieveLimits`]) и срок журнала автоответов.
///
/// ```yaml
/// meta:
///   sieve:
///     max_scripts: 16
///     max_script_size: 131072
///     vacation_log_ttl: 90d
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SieveConfig {
    /// Скриптов учётной записи.
    pub max_scripts: usize,
    /// Размер текста скрипта (и скрипта организации), байт.
    pub max_script_size: usize,
    /// Срок записей журнала автоответов — не меньше наибольшего периода `:days`, который допускает `tp-sieve`:
    /// более старые записи удаляет [`PgMeta::purge_vacation_log`].
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub vacation_log_ttl: Duration,
}

impl SieveConfig {
    /// Лимиты хранилища.
    pub fn limits(&self) -> SieveLimits {
        SieveLimits {
            max_scripts: self.max_scripts,
            max_script_size: self.max_script_size,
        }
    }

    /// Секция с лимитами `limits` и сроком журнала по умолчанию.
    pub fn with_limits(limits: SieveLimits) -> Self {
        Self {
            max_scripts: limits.max_scripts,
            max_script_size: limits.max_script_size,
            ..Self::default()
        }
    }
}

impl Default for SieveConfig {
    /// Лимиты [`SieveLimits::default`], журнал автоответов — 90 суток.
    fn default() -> Self {
        let l = SieveLimits::default();
        Self {
            max_scripts: l.max_scripts,
            max_script_size: l.max_script_size,
            vacation_log_ttl: Duration::from_secs(90 * 24 * 3600),
        }
    }
}

fn invalid_name(name: &str) -> Error {
    Error::invalid(format!("недопустимое имя скрипта: {name:?}"))
}

/// Состояние скриптов с блокировкой строки состояния ящика; нет её — `NotFound`.
async fn lock_state(tx: &mut PgConnection, scope: AccountScope) -> Result<u64> {
    let v: Option<i64> = sqlx::query_scalar(
        "SELECT sieve_state FROM mailbox_state WHERE tenant_id = $1 AND account_id = $2 FOR UPDATE",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    v.ok_or(Error::NotFound).and_then(to_u64)
}

/// Следующее состояние скриптов (строка уже заблокирована [`lock_state`]).
async fn bump_state(tx: &mut PgConnection, scope: AccountScope) -> Result<u64> {
    let v: i64 = sqlx::query_scalar(
        "UPDATE mailbox_state SET sieve_state = sieve_state + 1
         WHERE tenant_id = $1 AND account_id = $2 RETURNING sieve_state",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err)?;
    to_u64(v)
}

/// Скрипт есть (и активен ли он).
async fn script_active(
    tx: &mut PgConnection,
    scope: AccountScope,
    name: &str,
) -> Result<Option<bool>> {
    sqlx::query_scalar(
        "SELECT active FROM sieve_scripts WHERE tenant_id = $1 AND account_id = $2 AND name = $3",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(name)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)
}

#[derive(Debug, FromRow)]
struct VacationRow {
    enabled: bool,
    from_at: Option<OffsetDateTime>,
    to_at: Option<OffsetDateTime>,
    subject: Option<String>,
    text_body: Option<String>,
    html_body: Option<String>,
}

impl PgMeta {
    /// Изменение скриптов или автоответа учётной записи в одной транзакции на её шарде: `f` получает соединение
    /// с заблокированной строкой состояния и возвращает `true`, если изменение было (`create` — операция может
    /// быть первой у учётной записи на шарде). Состояние растёт, `SieveChanged` публикуется после фиксации.
    async fn sieve_change<F>(&self, scope: AccountScope, create: bool, f: F) -> Result<u64>
    where
        F: for<'c> FnOnce(&'c mut PgConnection) -> crate::pool::TxFuture<'c, bool> + Send,
    {
        let (state, changed) = self
            .once(async {
                let db = self.box_db(scope).await?;
                let mut tx = begin(&db.pool, scope.tenant).await?;
                if create && db.shard != ShardId::MAIN {
                    // Учётная запись есть в каталоге (шард найден), состояния ящика на шарде может ещё не быть.
                    ensure_state(&mut tx, scope).await?;
                }
                let cur = lock_state(&mut tx, scope).await?;
                if !f(&mut tx).await? {
                    return Ok((cur, false));
                }
                let next = bump_state(&mut tx, scope).await?;
                tx.commit().await.map_err(db_err)?;
                Ok((next, true))
            })
            .await?;
        if changed {
            self.signal(sieve_changed(scope)).await;
        }
        Ok(state)
    }

    /// Удаляет записи журнала автоответов старше `meta.sieve.vacation_log_ttl` на всех шардах (задание роли
    /// maintenance). Возвращает число удалённых.
    pub async fn purge_vacation_log(&self) -> Result<u64> {
        let ttl = time::Duration::try_from(self.cfg.sieve.vacation_log_ttl)
            .map_err(|_| Error::invalid("meta.sieve.vacation_log_ttl вне диапазона"))?;
        let before = OffsetDateTime::now_utc()
            .checked_sub(ttl)
            .ok_or_else(|| Error::invalid("meta.sieve.vacation_log_ttl вне диапазона"))?;
        self.purge_vacation_log_before(before).await
    }

    /// Удаляет записи журнала автоответов, отмеченные раньше `before`, на всех шардах.
    pub async fn purge_vacation_log_before(&self, before: OffsetDateTime) -> Result<u64> {
        // Удаление по сроку: повтор после обрыва удаляет только оставшееся.
        self.rewrite(|| async move {
            let mut total = 0;
            for db in self.all_shards().await? {
                total += with_system(&db.pool, move |tx| {
                    Box::pin(async move {
                        let r = sqlx::query("DELETE FROM vacation_log WHERE replied_at < $1")
                            .bind(before)
                            .execute(&mut *tx)
                            .await
                            .map_err(db_err)?;
                        Ok(r.rows_affected())
                    })
                })
                .await?;
            }
            Ok(total)
        })
        .await
    }
}

#[async_trait]
impl VacationLog for PgMeta {
    async fn seen(
        &self,
        scope: AccountScope,
        sender: &str,
        handle: &str,
        period: Duration,
    ) -> Result<bool> {
        let period = i64::try_from(period.as_micros()).unwrap_or(i64::MAX);
        // Одной вставкой: новая запись или запись старше периода — отметка и ответ; иначе ответ уже был. Параллельный
        // вызов ждёт блокировку строки и проверяет период по зафиксированной отметке — ответ получает один.
        self.once(async {
            let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
            let marked: Option<i32> = sqlx::query_scalar(
                "INSERT INTO vacation_log AS v (tenant_id, account_id, digest, replied_at)
                 VALUES ($1, $2, sha256(convert_to($3, 'UTF8') || '\\x00'::bytea || convert_to($4, 'UTF8')), now())
                 ON CONFLICT (tenant_id, account_id, digest) DO UPDATE SET replied_at = excluded.replied_at
                 WHERE v.replied_at <= excluded.replied_at - $5::float8 * interval '1 microsecond'
                 RETURNING 1",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(sender)
            .bind(handle)
            .bind(period)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            Ok(marked.is_none())
        })
        .await
    }
}

#[async_trait]
impl SieveStore for PgMeta {
    fn limits(&self) -> SieveLimits {
        self.cfg.sieve.limits()
    }

    async fn list(&self, scope: AccountScope) -> Result<Vec<ScriptInfo>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let rows: Vec<(String, i32, bool)> = sqlx::query_as(
                "SELECT name, octet_length(script), active FROM sieve_scripts
                 WHERE tenant_id = $1 AND account_id = $2 ORDER BY name COLLATE \"C\"",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            rows.into_iter()
                .map(|(name, size, active)| {
                    Ok(ScriptInfo {
                        name,
                        size: to_u64(i64::from(size))?,
                        active,
                    })
                })
                .collect()
        })
        .await
    }

    async fn get(&self, scope: AccountScope, name: &str) -> Result<Option<String>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let text: Option<String> = sqlx::query_scalar(
                "SELECT script FROM sieve_scripts WHERE tenant_id = $1 AND account_id = $2 AND name = $3",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(name)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            Ok(text)
        })
        .await
    }

    async fn active(&self, scope: AccountScope) -> Result<Option<ActiveScript>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let row: Option<(String, String)> = sqlx::query_as(
                "SELECT name, script FROM sieve_scripts WHERE tenant_id = $1 AND account_id = $2 AND active",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            Ok(row.map(|(name, text)| ActiveScript { name, text }))
        })
        .await
    }

    async fn put(&self, scope: AccountScope, name: &str, text: String) -> Result<u64> {
        if !valid_script_name(name) {
            return Err(invalid_name(name));
        }
        let limits = self.limits();
        if text.len() > limits.max_script_size {
            return Err(Error::QuotaExceeded);
        }
        let max = i64::try_from(limits.max_scripts).unwrap_or(i64::MAX);
        let name = name.to_owned();
        self.sieve_change(scope, true, move |tx| {
            Box::pin(async move {
                let (exists, count): (bool, i64) = sqlx::query_as(
                    "SELECT EXISTS (SELECT 1 FROM sieve_scripts
                                    WHERE tenant_id = $1 AND account_id = $2 AND name = $3),
                            (SELECT count(*) FROM sieve_scripts WHERE tenant_id = $1 AND account_id = $2)",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(&name)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
                if !exists && count >= max {
                    return Err(Error::QuotaExceeded);
                }
                // Замена текста сохраняет активность.
                sqlx::query(
                    "INSERT INTO sieve_scripts (tenant_id, account_id, name, script) VALUES ($1, $2, $3, $4)
                     ON CONFLICT (tenant_id, account_id, name)
                     DO UPDATE SET script = excluded.script, updated_at = now()",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(&name)
                .bind(&text)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                Ok(true)
            })
        })
        .await
    }

    async fn rename(&self, scope: AccountScope, from: &str, to: &str) -> Result<u64> {
        if !valid_script_name(to) {
            return Err(invalid_name(to));
        }
        let (from, to) = (from.to_owned(), to.to_owned());
        self.sieve_change(scope, false, move |tx| {
            Box::pin(async move {
                if script_active(tx, scope, &from).await?.is_none() {
                    return Err(Error::NotFound);
                }
                if from == to {
                    return Ok(false);
                }
                if script_active(tx, scope, &to).await?.is_some() {
                    return Err(Error::Conflict);
                }
                // Активность переходит к новому имени вместе со строкой.
                sqlx::query(
                    "UPDATE sieve_scripts SET name = $4, updated_at = now()
                     WHERE tenant_id = $1 AND account_id = $2 AND name = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(&from)
                .bind(&to)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                Ok(true)
            })
        })
        .await
    }

    async fn delete(&self, scope: AccountScope, name: &str) -> Result<u64> {
        let name = name.to_owned();
        self.sieve_change(scope, false, move |tx| {
            Box::pin(async move {
                match script_active(tx, scope, &name).await? {
                    None => return Err(Error::NotFound),
                    Some(true) => return Err(Error::Conflict),
                    Some(false) => {}
                }
                sqlx::query(
                    "DELETE FROM sieve_scripts WHERE tenant_id = $1 AND account_id = $2 AND name = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(&name)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                Ok(true)
            })
        })
        .await
    }

    async fn activate(&self, scope: AccountScope, name: Option<&str>) -> Result<u64> {
        let name = name.map(str::to_owned);
        self.sieve_change(scope, false, move |tx| {
            Box::pin(async move {
                if let Some(n) = &name
                    && script_active(tx, scope, n).await?.is_none()
                {
                    return Err(Error::NotFound);
                }
                // Сначала снять прежний активный: частичный уникальный индекс проверяется построчно.
                sqlx::query(
                    "UPDATE sieve_scripts SET active = false
                     WHERE tenant_id = $1 AND account_id = $2 AND active AND name IS DISTINCT FROM $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(&name)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                if let Some(n) = &name {
                    sqlx::query(
                        "UPDATE sieve_scripts SET active = true
                         WHERE tenant_id = $1 AND account_id = $2 AND name = $3",
                    )
                    .bind(scope.tenant.as_uuid())
                    .bind(scope.account.as_uuid())
                    .bind(n)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                }
                Ok(true)
            })
        })
        .await
    }

    async fn org_script(&self, tenant: TenantId) -> Result<Option<String>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            let text: Option<String> =
                sqlx::query_scalar("SELECT script FROM sieve_org_scripts WHERE tenant_id = $1")
                    .bind(tenant.as_uuid())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            Ok(text)
        })
        .await
    }

    async fn set_org_script(&self, tenant: TenantId, text: Option<String>) -> Result<()> {
        if text
            .as_ref()
            .is_some_and(|t| t.len() > self.limits().max_script_size)
        {
            return Err(Error::QuotaExceeded);
        }
        let text = &text;
        // Замена по значению: повтор после обрыва даёт то же состояние.
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            match text {
                Some(t) => sqlx::query(
                    "INSERT INTO sieve_org_scripts (tenant_id, script) VALUES ($1, $2)
                     ON CONFLICT (tenant_id) DO UPDATE SET script = excluded.script, updated_at = now()",
                )
                .bind(tenant.as_uuid())
                .bind(t),
                None => sqlx::query("DELETE FROM sieve_org_scripts WHERE tenant_id = $1")
                    .bind(tenant.as_uuid()),
            }
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn vacation(&self, scope: AccountScope) -> Result<Vacation> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let row: Option<VacationRow> = sqlx::query_as(
                "SELECT enabled, from_at, to_at, subject, text_body, html_body FROM vacation
                 WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            Ok(row.map_or_else(Vacation::default, |r| Vacation {
                enabled: r.enabled,
                from: r.from_at,
                to: r.to_at,
                subject: r.subject,
                text_body: r.text_body,
                html_body: r.html_body,
            }))
        })
        .await
    }

    async fn set_vacation(&self, scope: AccountScope, vacation: Vacation) -> Result<u64> {
        vacation.validate()?;
        self.sieve_change(scope, true, move |tx| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO vacation (tenant_id, account_id, enabled, from_at, to_at, subject, text_body,
                                           html_body)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                     ON CONFLICT (tenant_id, account_id) DO UPDATE SET
                         enabled = excluded.enabled, from_at = excluded.from_at, to_at = excluded.to_at,
                         subject = excluded.subject, text_body = excluded.text_body,
                         html_body = excluded.html_body, updated_at = now()",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(vacation.enabled)
                .bind(vacation.from)
                .bind(vacation.to)
                .bind(&vacation.subject)
                .bind(&vacation.text_body)
                .bind(&vacation.html_body)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                Ok(true)
            })
        })
        .await
    }

    async fn state(&self, scope: AccountScope) -> Result<u64> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let v: Option<i64> = sqlx::query_scalar(
                "SELECT sieve_state FROM mailbox_state WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            to_u64(v.unwrap_or(0))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_yaml() {
        let c: SieveConfig = serde_yaml_ng::from_str(
            "max_scripts: 3\nmax_script_size: 1024\nvacation_log_ttl: 30d\n",
        )
        .expect("yaml");
        assert_eq!(
            c.limits(),
            SieveLimits {
                max_scripts: 3,
                max_script_size: 1024
            }
        );
        assert_eq!(c.vacation_log_ttl, Duration::from_secs(30 * 24 * 3600));
        assert_eq!(SieveConfig::default().limits(), SieveLimits::default());
        assert!(serde_yaml_ng::from_str::<SieveConfig>("max_script: 1\n").is_err());
    }
}
