//! Журнал аудита Admin API на PostgreSQL (CONTRACTS.md#AdminAuditLog; META-22, META-70; SRS 5.2).
//!
//! Таблица `audit_log` — только добавление: UPDATE и DELETE отклоняет триггер, роль приложения имеет только
//! SELECT и INSERT. Запись целиком — `record` (jsonb), колонки `at`, `tenant_id`, `actor_tenant`, `actor` — для
//! выборки и RLS. Запись организации — в её транзакции, уровня инсталляции (`tenant_id` NULL) — в системной.
//! Добавление не повторяется над `Failover`: повтор после обрыва во время COMMIT записал бы запись дважды.

use async_trait::async_trait;
use sqlx::PgConnection;
use sqlx::types::Json;
use tp_contracts::admin_audit::{
    AdminAuditEntry, AdminAuditPage, AdminAuditQuery, AdminAuditRecord, AuditLog,
};
use tp_contracts::{Error, Result, TenantId};

use crate::PgMeta;
use crate::pool::{TxFuture, with_system, with_tenant};
use crate::schema::{db_err, to_u64};

const INSERT: &str = "INSERT INTO audit_log (at, tenant_id, actor_tenant, actor, record)
                      VALUES ($1, $2, $3, $4, $5) RETURNING seq";

/// Добавление записи в транзакции, которую открывает вызывающий; номер записи.
fn insert(
    record: AdminAuditRecord,
) -> impl for<'c> FnOnce(&'c mut PgConnection) -> TxFuture<'c, i64> {
    move |tx| {
        Box::pin(async move {
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
        let rows: Vec<(i64, Json<AdminAuditRecord>)> = self
            .idempotent(|| {
                with_tenant(&self.pool, tenant, move |tx| {
                    Box::pin(async move {
                        sqlx::query_as(
                            "SELECT seq, record FROM audit_log
                             WHERE tenant_id = $1
                               AND ($2::timestamptz IS NULL OR at >= $2)
                               AND ($3::timestamptz IS NULL OR at < $3)
                               AND ($4::bigint IS NULL OR seq > $4)
                             ORDER BY seq LIMIT $5",
                        )
                        .bind(tenant.as_uuid())
                        .bind(since)
                        .bind(until)
                        .bind(after)
                        .bind(limit)
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
}
