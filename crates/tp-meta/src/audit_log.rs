//! Журнал аудита Admin API на PostgreSQL (CONTRACTS.md#AdminAuditLog; META-22, META-70; SRS 5.2).
//!
//! Таблица `audit_log` — только добавление: UPDATE и DELETE отклоняет триггер, роль приложения имеет только
//! SELECT и INSERT. Запись целиком — `record` (jsonb), колонки `at`, `tenant_id`, `actor_tenant`, `actor` — для
//! выборки и RLS. Запись организации — в её транзакции, уровня инсталляции (`tenant_id` NULL) — в системной.
//! Добавление не повторяется над `Failover`: повтор после обрыва во время COMMIT записал бы запись дважды.
//!
//! Выгрузка в SIEM (APP-59) читает записи после отметки доставки приёмника (таблица `audit_export`). Чтобы отметка
//! «доставлено до N» ничего не пропускала, добавления идут по одному под рекомендательной блокировкой до фиксации:
//! номер выдаётся под ней, поэтому записи становятся видимы строго в порядке номеров (журнал аудита пишется редко —
//! действия администраторов).

use async_trait::async_trait;
use sqlx::PgConnection;
use sqlx::types::Json;
use tp_contracts::admin_audit::{
    AdminAuditEntry, AdminAuditPage, AdminAuditQuery, AdminAuditRecord, AuditLog, check_export_mark,
};
use tp_contracts::{Error, Result, TenantId};

use crate::PgMeta;
use crate::pool::{TxFuture, with_system, with_tenant};
use crate::schema::{db_err, to_i64, to_u64};

const INSERT: &str = "INSERT INTO audit_log (at, tenant_id, actor_tenant, actor, record)
                      VALUES ($1, $2, $3, $4, $5) RETURNING seq";

/// Блокировка порядка добавлений до конца транзакции.
const ORDER_LOCK: &str = "SELECT pg_advisory_xact_lock(hashtextextended('thepost:audit_log', 0))";

/// Добавление записи в транзакции, которую открывает вызывающий; номер записи.
fn insert(
    record: AdminAuditRecord,
) -> impl for<'c> FnOnce(&'c mut PgConnection) -> TxFuture<'c, i64> {
    move |tx| {
        Box::pin(async move {
            sqlx::query(ORDER_LOCK)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            sqlx::query_scalar(INSERT)
                .bind(record.at)
                .bind(record.tenant.map(|t| *t.as_uuid()))
                .bind(record.actor_tenant.as_uuid())
                .bind(record.actor.as_uuid())
                .bind(Json(&record))
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)
        })
    }
}

#[async_trait]
impl AuditLog for PgMeta {
    async fn append(&self, record: &AdminAuditRecord) -> Result<u64> {
        let seq = match record.tenant {
            Some(tenant) => with_tenant(&self.pool, tenant, insert(record.clone())).await?,
            None => with_system(&self.pool, insert(record.clone())).await?,
        };
        to_u64(seq)
    }

    async fn search(&self, tenant: TenantId, query: &AdminAuditQuery) -> Result<AdminAuditPage> {
        query.validate()?;
        let limit = i64::try_from(query.limit).unwrap_or(i64::MAX);
        let after = query
            .after
            .map(i64::try_from)
            .transpose()
            .map_err(|_| Error::invalid("audit: after out of range"))?;
        let (since, until) = (query.since, query.until);
        let (actor, method, route) = (&query.actor, &query.method, &query.route_prefix);
        let rows: Vec<(i64, Json<AdminAuditRecord>)> = self
            .idempotent(|| {
                let (actor, method, route) = (actor.clone(), method.clone(), route.clone());
                with_tenant(&self.pool, tenant, move |tx| {
                    Box::pin(async move {
                        // Условия по записи — правило `AdminAuditQuery::matches`.
                        sqlx::query_as(
                            "SELECT seq, record FROM audit_log
                             WHERE tenant_id = $1
                               AND ($2::timestamptz IS NULL OR at >= $2)
                               AND ($3::timestamptz IS NULL OR at < $3)
                               AND ($4::bigint IS NULL OR seq > $4)
                               AND ($6::text IS NULL OR lower(record->>'actor_login') = lower($6)
                                    OR actor::text = $6)
                               AND ($7::text IS NULL OR record->>'method' = $7)
                               AND ($8::text IS NULL OR starts_with(record->>'route', $8))
                             ORDER BY seq LIMIT $5",
                        )
                        .bind(tenant.as_uuid())
                        .bind(since)
                        .bind(until)
                        .bind(after)
                        .bind(limit)
                        .bind(actor)
                        .bind(method)
                        .bind(route)
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(db_err)
                    })
                })
            })
            .await?;
        let entries = rows
            .into_iter()
            .map(|(seq, Json(record))| {
                Ok(AdminAuditEntry {
                    seq: to_u64(seq)?,
                    record,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let next = (entries.len() == query.limit)
            .then(|| entries.last().map(|e| e.seq))
            .flatten();
        Ok(AdminAuditPage { entries, next })
    }

    async fn export_page(&self, after: u64, limit: usize) -> Result<Vec<AdminAuditEntry>> {
        if limit == 0 {
            return Err(Error::invalid("audit export: limit must be at least 1"));
        }
        let after = to_i64(after)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows: Vec<(i64, Json<AdminAuditRecord>)> = self
            .idempotent(|| {
                with_system(&self.pool, move |tx| {
                    Box::pin(async move {
                        sqlx::query_as(
                            "SELECT seq, record FROM audit_log WHERE seq > $1 ORDER BY seq LIMIT $2",
                        )
                        .bind(after)
                        .bind(limit)
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(db_err)
                    })
                })
            })
            .await?;
        rows.into_iter()
            .map(|(seq, Json(record))| {
                Ok(AdminAuditEntry {
                    seq: to_u64(seq)?,
                    record,
                })
            })
            .collect()
    }

    async fn export_mark(&self, sink: &str) -> Result<u64> {
        let seq: Option<i64> = self
            .idempotent(|| {
                let sink = sink.to_owned();
                with_system(&self.pool, move |tx| {
                    Box::pin(async move {
                        sqlx::query_scalar("SELECT delivered_seq FROM audit_export WHERE sink = $1")
                            .bind(sink)
                            .fetch_optional(&mut *tx)
                            .await
                            .map_err(db_err)
                    })
                })
            })
            .await?;
        seq.map_or(Ok(0), to_u64)
    }

    async fn advance_export_mark(&self, sink: &str, from: u64, to: u64) -> Result<bool> {
        check_export_mark(sink, from, to)?;
        let (from, to) = (to_i64(from)?, to_i64(to)?);
        // Повтор после обрыва во время COMMIT безопасен: отметка уже сдвинута — ответ `false`.
        let moved: Option<String> = self
            .rewrite(|| {
                let sink = sink.to_owned();
                with_system(&self.pool, move |tx| {
                    Box::pin(async move {
                        let q = if from == 0 {
                            sqlx::query_scalar(
                                "INSERT INTO audit_export (sink, delivered_seq) VALUES ($1, $3)
                                 ON CONFLICT (sink) DO UPDATE
                                     SET delivered_seq = EXCLUDED.delivered_seq, updated_at = now()
                                     WHERE audit_export.delivered_seq = $2
                                 RETURNING sink",
                            )
                        } else {
                            sqlx::query_scalar(
                                "UPDATE audit_export SET delivered_seq = $3, updated_at = now()
                                 WHERE sink = $1 AND delivered_seq = $2
                                 RETURNING sink",
                            )
                        };
                        q.bind(sink)
                            .bind(from)
                            .bind(to)
                            .fetch_optional(&mut *tx)
                            .await
                            .map_err(db_err)
                    })
                })
            })
            .await?;
        Ok(moved.is_some())
    }
}
