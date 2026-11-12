//! Остановка синхронизации каталога на PostgreSQL (CONTRACTS.md#Directory, `SyncGate`; CTR-35): запись организации
//! в `sync_holds` (основная БД, миграция `0032_sync_holds`). Подтверждение сохраняется при повторной остановке,
//! только если удаляемых не больше, — в том же `UPDATE`, поэтому повтор операции даёт тот же результат.

use async_trait::async_trait;
use sqlx::FromRow;
use time::OffsetDateTime;
use tp_contracts::auth::{MassRemoval, SyncGate};
use tp_contracts::{Error, Result, TenantId};

use crate::PgMeta;
use crate::schema::{begin, db_err, to_i64, to_u64};
use crate::shards::ShardId;

#[derive(FromRow)]
struct HoldRow {
    removed: i64,
    total: i64,
    detected_at: OffsetDateTime,
    approved: bool,
}

impl HoldRow {
    fn into_hold(self) -> Result<MassRemoval> {
        Ok(MassRemoval {
            removed: to_u64(self.removed)?,
            total: to_u64(self.total)?,
            detected_at: self.detected_at,
            approved: self.approved,
        })
    }
}

const RETURNING: &str = "RETURNING removed, total, detected_at, approved";

#[async_trait]
impl SyncGate for PgMeta {
    async fn hold_sync(
        &self,
        tenant: TenantId,
        removed: u64,
        total: u64,
        at: OffsetDateTime,
    ) -> Result<MassRemoval> {
        let (removed, total) = (to_i64(removed)?, to_i64(total)?);
        self.idempotent(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            let sql = format!(
                "INSERT INTO sync_holds (tenant_id, removed, total, detected_at, approved) VALUES ($1, $2, $3, $4, false)
                 ON CONFLICT (tenant_id) DO UPDATE
                     SET removed = EXCLUDED.removed, total = EXCLUDED.total, detected_at = EXCLUDED.detected_at,
                         approved = sync_holds.approved AND EXCLUDED.removed <= sync_holds.removed
                 {RETURNING}"
            );
            let row: HoldRow = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                .bind(tenant.as_uuid())
                .bind(removed)
                .bind(total)
                .bind(at)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            row.into_hold()
        })
        .await
    }

    async fn pending_sync(&self, tenant: TenantId) -> Result<Option<MassRemoval>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let row: Option<HoldRow> = sqlx::query_as(
                "SELECT removed, total, detected_at, approved FROM sync_holds WHERE tenant_id = $1",
            )
            .bind(tenant.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            row.map(HoldRow::into_hold).transpose()
        })
        .await
    }

    async fn approve_sync(&self, tenant: TenantId) -> Result<MassRemoval> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            let sql =
                format!("UPDATE sync_holds SET approved = true WHERE tenant_id = $1 {RETURNING}");
            let row: Option<HoldRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                .bind(tenant.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            row.ok_or(Error::NotFound)?.into_hold()
        })
        .await
    }

    async fn clear_sync(&self, tenant: TenantId) -> Result<()> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            sqlx::query("DELETE FROM sync_holds WHERE tenant_id = $1")
                .bind(tenant.as_uuid())
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            crate::commits::commit(tx).await
        })
        .await
    }
}
