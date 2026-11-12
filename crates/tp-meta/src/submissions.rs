//! Отправки JMAP на PostgreSQL (x/jmap-08; CONTRACTS.md#Identity и EmailSubmission; CTR-30, CTR-42).
//!
//! Записи `email_submissions` — таблица ящика на шарде учётной записи; состояние (`submission_state`) — в строке
//! `mailbox_state`, каждое изменение блокирует её и увеличивает состояние: изменения одной учётной записи
//! упорядочены, отмена и фиксация одной отправки взаимоисключающие. Удалённые записи оставляют надгробие
//! (`email_submission_tombstones`) для `EmailSubmission/changes`; надгробия старше срока хранения удаляет
//! [`SubmissionStore::expire_submissions`] и поднимает нижнюю границу истории (`submission_floor`): изменения от
//! более раннего состояния — `Conflict`. Срок хранения и размер прохода роли maintenance — [`SubmissionsConfig`],
//! [`PgMeta::expire_old_submissions`].

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection};
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::meta::AccountScope;
use tp_contracts::queue::SubmitStatus;
use tp_contracts::submission::{EmailSubmission, SubmissionChanges, SubmissionStore, UndoStatus};
use tp_contracts::{AccountId, Error, MessageId, QueueId, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::mailboxes::ensure_state;
use crate::pool::{DbPool, with_system};
use crate::schema::{begin, db_err, to_u64};
use crate::shards::ShardId;

/// Отправки: секция `meta.submissions` (SRS 5.5).
///
/// ```yaml
/// submissions:
///   retention: 30d
///   expire_batch: 1000
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SubmissionsConfig {
    /// Срок хранения завершённых и отменённых отправок (по `sendAt`) и надгробий удалённых.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retention: Duration,
    /// Не более записей за проход роли maintenance; остальные — в следующий проход.
    pub expire_batch: usize,
}

impl Default for SubmissionsConfig {
    fn default() -> Self {
        Self {
            retention: Duration::from_secs(30 * 24 * 3600),
            expire_batch: 1_000,
        }
    }
}

fn undo_str(u: UndoStatus) -> &'static str {
    match u {
        UndoStatus::Pending => "pending",
        UndoStatus::Final => "final",
        UndoStatus::Canceled => "canceled",
    }
}

fn undo_of(s: &str) -> Result<UndoStatus> {
    Ok(match s {
        "pending" => UndoStatus::Pending,
        "final" => UndoStatus::Final,
        "canceled" => UndoStatus::Canceled,
        other => return Err(Error::internal(format!("undoStatus отправки: {other}"))),
    })
}

fn delivery_json(delivery: &[(String, SubmitStatus)]) -> Result<serde_json::Value> {
    serde_json::to_value(delivery).map_err(|e| Error::internal(format!("deliveryStatus: {e}")))
}

fn limit_i64(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

#[derive(FromRow)]
struct Row {
    id: String,
    identity: String,
    email_id: Uuid,
    mail_from: String,
    rcpt_to: Vec<String>,
    send_at: OffsetDateTime,
    undo_status: String,
    delivery: serde_json::Value,
    queue_id: Option<Uuid>,
}

impl Row {
    fn into_submission(self) -> Result<EmailSubmission> {
        Ok(EmailSubmission {
            id: self.id,
            identity: self.identity,
            email: MessageId::from_uuid(self.email_id),
            mail_from: self.mail_from,
            rcpt_to: self.rcpt_to,
            send_at: self.send_at,
            undo_status: undo_of(&self.undo_status)?,
            delivery: serde_json::from_value(self.delivery)
                .map_err(|e| Error::internal(format!("deliveryStatus: {e}")))?,
            queue: self.queue_id.map(QueueId::from_uuid),
        })
    }
}

/// Столбцы [`Row`] (литерал: запросы собираются `concat!`).
macro_rules! columns {
    () => {
        "id, identity, email_id, mail_from, rcpt_to, send_at, undo_status, delivery, queue_id"
    };
}

/// Состояние отправок с блокировкой строки состояния ящика (нет её — создаётся: на шарде, кроме основной БД,
/// её может ещё не быть).
async fn lock_state(tx: &mut PgConnection, shard: ShardId, scope: AccountScope) -> Result<i64> {
    if shard != ShardId::MAIN {
        ensure_state(tx, scope).await?;
    }
    let v: Option<i64> = sqlx::query_scalar(
        "SELECT submission_state FROM mailbox_state
         WHERE tenant_id = $1 AND account_id = $2 FOR UPDATE",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    v.ok_or(Error::NotFound)
}

/// Записывает новое состояние `state` (строка заблокирована [`lock_state`]).
async fn set_state(tx: &mut PgConnection, scope: AccountScope, state: i64) -> Result<u64> {
    sqlx::query(
        "UPDATE mailbox_state SET submission_state = $3 WHERE tenant_id = $1 AND account_id = $2",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(state)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    to_u64(state)
}

async fn fetch(tx: &mut PgConnection, scope: AccountScope, id: &str) -> Result<Option<Row>> {
    sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM email_submissions WHERE tenant_id = $1 AND account_id = $2 AND id = $3"
    ))
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)
}

/// Удаляет отправки `ids` учётной записи (строка состояния заблокирована, текущее состояние — `state`) и
/// оставляет надгробия; возвращает новое состояние и число удалённых.
async fn destroy(
    tx: &mut PgConnection,
    scope: AccountScope,
    state: i64,
    ids: &[String],
) -> Result<(i64, usize)> {
    let gone: Vec<(String, i64)> = sqlx::query_as(
        "DELETE FROM email_submissions WHERE tenant_id = $1 AND account_id = $2 AND id = ANY($3)
         RETURNING id, created_state",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    let mut state = state;
    for (id, created) in &gone {
        state += 1;
        sqlx::query(
            "INSERT INTO email_submission_tombstones
                 (tenant_id, account_id, id, created_state, destroyed_state, destroyed_at)
             VALUES ($1, $2, $3, $4, $5, now())
             ON CONFLICT (tenant_id, account_id, id) DO UPDATE
                 SET created_state = EXCLUDED.created_state, destroyed_state = EXCLUDED.destroyed_state,
                     destroyed_at = EXCLUDED.destroyed_at",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(id)
        .bind(created)
        .bind(state)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    Ok((state, gone.len()))
}

impl PgMeta {
    /// Изменение отправок учётной записи одной транзакцией на её шарде: `f` получает соединение с заблокированной
    /// строкой состояния и текущее состояние, возвращает результат и новое состояние (равное текущему — без
    /// изменения).
    async fn submission_change<T, F>(&self, scope: AccountScope, f: F) -> Result<T>
    where
        T: Send,
        F: for<'c> FnOnce(&'c mut PgConnection, i64) -> crate::pool::TxFuture<'c, (T, i64)> + Send,
    {
        self.once(async {
            let db = self.box_db(scope).await?;
            let mut tx = begin(&db.pool, scope.tenant).await?;
            let cur = lock_state(&mut tx, db.shard, scope).await?;
            let (out, next) = f(&mut tx, cur).await?;
            if next != cur {
                set_state(&mut tx, scope, next).await?;
            }
            crate::commits::commit(tx).await?;
            Ok(out)
        })
        .await
    }

    /// Удаляет завершённые и отменённые отправки старше `meta.submissions.retention` и надгробия того же срока
    /// (задание роли maintenance), не более `meta.submissions.expire_batch` отправок за проход. Возвращает число
    /// удалённых отправок.
    pub async fn expire_old_submissions(&self) -> Result<u64> {
        let retention = time::Duration::try_from(self.cfg.submissions.retention)
            .map_err(|_| Error::invalid("meta.submissions.retention вне диапазона"))?;
        let before = OffsetDateTime::now_utc()
            .checked_sub(retention)
            .ok_or_else(|| Error::invalid("meta.submissions.retention вне диапазона"))?;
        let n = self
            .expire_submissions(before, self.cfg.submissions.expire_batch)
            .await?;
        Ok(u64::try_from(n).unwrap_or(u64::MAX))
    }

    /// Удаляет надгробия отправок, удалённых раньше `before`, на шарде `pool` и поднимает нижнюю границу истории
    /// изменений учётных записей.
    async fn purge_submission_tombstones(
        &self,
        pool: &DbPool,
        before: OffsetDateTime,
        limit: usize,
    ) -> Result<()> {
        with_system(pool, move |tx| {
            Box::pin(async move {
                sqlx::query(
                    "WITH gone AS (
                         DELETE FROM email_submission_tombstones
                         WHERE (tenant_id, account_id, id) IN (
                             SELECT tenant_id, account_id, id FROM email_submission_tombstones
                             WHERE destroyed_at < $1 ORDER BY destroyed_at LIMIT $2)
                         RETURNING tenant_id, account_id, destroyed_state),
                     top AS (
                         SELECT tenant_id, account_id, max(destroyed_state) AS floor FROM gone
                         GROUP BY tenant_id, account_id)
                     UPDATE mailbox_state m SET submission_floor = greatest(m.submission_floor, top.floor)
                     FROM top WHERE m.tenant_id = top.tenant_id AND m.account_id = top.account_id",
                )
                .bind(before)
                .bind(limit_i64(limit))
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                Ok(())
            })
        })
        .await
    }
}

#[async_trait]
impl SubmissionStore for PgMeta {
    async fn insert(&self, scope: AccountScope, submission: EmailSubmission) -> Result<u64> {
        submission.validate()?;
        if submission.undo_status == UndoStatus::Canceled {
            return Err(Error::invalid("новая отправка уже отменена"));
        }
        let delivery = delivery_json(&submission.delivery)?;
        self.submission_change(scope, move |tx, cur| {
            Box::pin(async move {
                let next = cur + 1;
                let inserted = sqlx::query(
                    "INSERT INTO email_submissions (tenant_id, account_id, id, identity, email_id, mail_from,
                                                    rcpt_to, send_at, undo_status, delivery, queue_id,
                                                    created_state, changed_state)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12)
                     ON CONFLICT DO NOTHING",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(&submission.id)
                .bind(&submission.identity)
                .bind(submission.email.as_uuid())
                .bind(&submission.mail_from)
                .bind(&submission.rcpt_to)
                .bind(submission.send_at)
                .bind(undo_str(submission.undo_status))
                .bind(&delivery)
                .bind(submission.queue.map(|q| *q.as_uuid()))
                .bind(next)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                if inserted.rows_affected() == 0 {
                    return Err(Error::Conflict);
                }
                // Идентификатор снова занят: прежнее надгробие больше не нужно.
                sqlx::query(
                    "DELETE FROM email_submission_tombstones
                     WHERE tenant_id = $1 AND account_id = $2 AND id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(&submission.id)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                Ok((to_u64(next)?, next))
            })
        })
        .await
    }

    async fn get(&self, scope: AccountScope, id: &str) -> Result<Option<EmailSubmission>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
            let row = fetch(&mut tx, scope, id).await?;
            crate::commits::commit(tx).await?;
            row.map(Row::into_submission).transpose()
        })
        .await
    }

    async fn list(&self, scope: AccountScope) -> Result<Vec<EmailSubmission>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
            let rows: Vec<Row> = sqlx::query_as(concat!(
                "SELECT ",
                columns!(),
                " FROM email_submissions WHERE tenant_id = $1 AND account_id = $2
                 ORDER BY send_at, id COLLATE \"C\""
            ))
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            rows.into_iter().map(Row::into_submission).collect()
        })
        .await
    }

    async fn cancel(&self, scope: AccountScope, id: &str) -> Result<EmailSubmission> {
        let id = id.to_owned();
        self.submission_change(scope, move |tx, cur| {
            Box::pin(async move {
                let mut s = fetch(tx, scope, &id)
                    .await?
                    .ok_or(Error::NotFound)?
                    .into_submission()?;
                match s.undo_status {
                    UndoStatus::Canceled => return Ok((s, cur)),
                    UndoStatus::Final => return Err(Error::Conflict),
                    UndoStatus::Pending => s.undo_status = UndoStatus::Canceled,
                }
                let next = cur + 1;
                sqlx::query(
                    "UPDATE email_submissions SET undo_status = 'canceled', changed_state = $4
                     WHERE tenant_id = $1 AND account_id = $2 AND id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(&id)
                .bind(next)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                Ok((s, next))
            })
        })
        .await
    }

    async fn finalize(
        &self,
        scope: AccountScope,
        id: &str,
        delivery: Vec<(String, SubmitStatus)>,
        queue: Option<QueueId>,
    ) -> Result<u64> {
        let id = id.to_owned();
        let delivery = delivery_json(&delivery)?;
        self.submission_change(scope, move |tx, cur| {
            Box::pin(async move {
                let undo: Option<String> = sqlx::query_scalar(
                    "SELECT undo_status FROM email_submissions
                     WHERE tenant_id = $1 AND account_id = $2 AND id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(&id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
                if undo_of(&undo.ok_or(Error::NotFound)?)? == UndoStatus::Canceled {
                    return Err(Error::Conflict);
                }
                let next = cur + 1;
                sqlx::query(
                    "UPDATE email_submissions
                     SET undo_status = 'final', delivery = $4, queue_id = $5, changed_state = $6
                     WHERE tenant_id = $1 AND account_id = $2 AND id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(&id)
                .bind(&delivery)
                .bind(queue.map(|q| *q.as_uuid()))
                .bind(next)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                Ok((to_u64(next)?, next))
            })
        })
        .await
    }

    async fn delete(&self, scope: AccountScope, id: &str) -> Result<u64> {
        let ids = vec![id.to_owned()];
        self.submission_change(scope, move |tx, cur| {
            Box::pin(async move {
                let (next, n) = destroy(tx, scope, cur, &ids).await?;
                if n == 0 {
                    return Err(Error::NotFound);
                }
                Ok((to_u64(next)?, next))
            })
        })
        .await
    }

    async fn due(
        &self,
        now: OffsetDateTime,
        limit: usize,
    ) -> Result<Vec<(AccountScope, EmailSubmission)>> {
        #[derive(FromRow)]
        struct DueRow {
            tenant_id: Uuid,
            account_id: Uuid,
            #[sqlx(flatten)]
            row: Row,
        }
        self.idempotent(|| async move {
            let mut out = Vec::new();
            for db in self.all_shards().await? {
                let rows: Vec<DueRow> = with_system(&db.pool, move |tx| {
                    Box::pin(async move {
                        sqlx::query_as(concat!(
                            "SELECT tenant_id, account_id, ",
                            columns!(),
                            " FROM email_submissions
                             WHERE undo_status = 'pending' AND send_at <= $1
                             ORDER BY send_at, id COLLATE \"C\" LIMIT $2"
                        ))
                        .bind(now)
                        .bind(limit_i64(limit))
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(db_err)
                    })
                })
                .await?;
                for r in rows {
                    let scope = AccountScope {
                        tenant: TenantId::from_uuid(r.tenant_id),
                        account: AccountId::from_uuid(r.account_id),
                    };
                    out.push((scope, r.row.into_submission()?));
                }
            }
            out.sort_by(|a: &(AccountScope, EmailSubmission), b| {
                (a.1.send_at, &a.1.id).cmp(&(b.1.send_at, &b.1.id))
            });
            out.truncate(limit);
            Ok(out)
        })
        .await
    }

    async fn state(&self, scope: AccountScope) -> Result<u64> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
            let v: Option<i64> = sqlx::query_scalar(
                "SELECT submission_state FROM mailbox_state WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            to_u64(v.unwrap_or(0))
        })
        .await
    }

    /// Изменения — записи с `changed_state > since` и надгробия с `destroyed_state > since` (созданные после
    /// `since` пропускаются) по возрастанию состояния последнего изменения; у каждой отправки — одно, последнее.
    async fn submission_changes(
        &self,
        scope: AccountScope,
        since: u64,
        limit: usize,
    ) -> Result<SubmissionChanges> {
        if limit == 0 {
            return Err(Error::invalid("submission changes limit must be positive"));
        }
        let since = i64::try_from(since).map_err(|_| Error::Conflict)?;
        self.idempotent(|| async move {
            let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
            let bounds: Option<(i64, i64)> = sqlx::query_as(
                "SELECT submission_state, submission_floor FROM mailbox_state
                 WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            let (state, floor) = bounds.unwrap_or((0, 0));
            if since > state || since < floor {
                return Err(Error::Conflict);
            }
            let rows: Vec<(String, i64, i64, bool)> = sqlx::query_as(
                "SELECT id, created_state, changed_state AS st, false AS gone FROM email_submissions
                 WHERE tenant_id = $1 AND account_id = $2 AND changed_state > $3
                 UNION ALL
                 SELECT id, created_state, destroyed_state, true FROM email_submission_tombstones
                 WHERE tenant_id = $1 AND account_id = $2 AND destroyed_state > $3 AND created_state <= $3
                 ORDER BY st LIMIT $4",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(since)
            .bind(limit_i64(limit).saturating_add(1))
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            let has_more = rows.len() > limit;
            let rows = &rows[..rows.len().min(limit)];
            let new_state = match rows.last() {
                Some((_, _, st, _)) if has_more => *st,
                _ => state,
            };
            let mut out = SubmissionChanges {
                new_state: to_u64(new_state)?,
                has_more,
                ..SubmissionChanges::default()
            };
            for (id, created, _, gone) in rows {
                let list = if *gone {
                    &mut out.destroyed
                } else if *created > since {
                    &mut out.created
                } else {
                    &mut out.updated
                };
                list.push(id.clone());
            }
            out.created.sort_unstable();
            out.updated.sort_unstable();
            out.destroyed.sort_unstable();
            Ok(out)
        })
        .await
    }

    /// По шардам: выбор старых записей, затем удаление по учётным записям с блокировкой строки состояния (порядок
    /// блокировок — как у остальных изменений). Надгробия того же срока удаляются следом.
    async fn expire_submissions(&self, before: OffsetDateTime, limit: usize) -> Result<usize> {
        let mut total = 0;
        for db in self.all_shards().await? {
            if total >= limit {
                break;
            }
            let rest = limit - total;
            let old: Vec<(Uuid, Uuid, String)> = with_system(&db.pool, move |tx| {
                Box::pin(async move {
                    sqlx::query_as(
                        "SELECT tenant_id, account_id, id FROM email_submissions
                         WHERE undo_status <> 'pending' AND send_at < $1
                         ORDER BY send_at, id COLLATE \"C\" LIMIT $2",
                    )
                    .bind(before)
                    .bind(limit_i64(rest))
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(db_err)
                })
            })
            .await?;
            let mut by_account: BTreeMap<(Uuid, Uuid), Vec<String>> = BTreeMap::new();
            for (tenant, account, id) in old {
                by_account.entry((tenant, account)).or_default().push(id);
            }
            for ((tenant, account), ids) in by_account {
                let scope = AccountScope {
                    tenant: TenantId::from_uuid(tenant),
                    account: AccountId::from_uuid(account),
                };
                total += self
                    .once(async {
                        let mut tx = begin(&db.pool, scope.tenant).await?;
                        let cur = lock_state(&mut tx, db.shard, scope).await?;
                        // Повторная проверка под блокировкой: запись могла измениться после выбора.
                        let still: Vec<String> = sqlx::query_scalar(
                            "SELECT id FROM email_submissions
                             WHERE tenant_id = $1 AND account_id = $2 AND id = ANY($3)
                               AND undo_status <> 'pending' AND send_at < $4",
                        )
                        .bind(scope.tenant.as_uuid())
                        .bind(scope.account.as_uuid())
                        .bind(&ids)
                        .bind(before)
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(db_err)?;
                        let (next, n) = destroy(&mut tx, scope, cur, &still).await?;
                        if n > 0 {
                            set_state(&mut tx, scope, next).await?;
                        }
                        crate::commits::commit(tx).await?;
                        Ok(n)
                    })
                    .await?;
            }
            self.purge_submission_tombstones(&db.pool, before, limit)
                .await?;
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_roundtrip() {
        for u in [UndoStatus::Pending, UndoStatus::Final, UndoStatus::Canceled] {
            assert_eq!(undo_of(undo_str(u)), Ok(u));
        }
        assert!(undo_of("x").is_err());
    }

    #[test]
    fn delivery_roundtrip() {
        let d = vec![
            ("a@example.org".to_owned(), SubmitStatus::Queued),
            (
                "b@example.org".to_owned(),
                SubmitStatus::Failed("550".into()),
            ),
        ];
        let v = delivery_json(&d).expect("json");
        assert_eq!(
            serde_json::from_value::<Vec<(String, SubmitStatus)>>(v).expect("parse"),
            d
        );
    }

    #[test]
    fn config_from_yaml() {
        let c: SubmissionsConfig =
            serde_yaml_ng::from_str("retention: 7d\nexpire_batch: 10\n").expect("yaml");
        assert_eq!(c.retention, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(c.expire_batch, 10);
    }
}
