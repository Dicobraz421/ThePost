//! Манифесты поисковых индексов на PostgreSQL (CONTRACTS.md#Search, SRS 4.6).
//!
//! Строка `index_manifests` на учётную запись; запись манифеста — сравнение с обменом по версии в одном
//! операторе (`UPDATE … WHERE version = expected` или вставка при `expected = 0`): из конкурентных записей
//! с одной ожидаемой версией побеждает одна, остальные получают `Conflict`. Запрос перестройки тоже
//! увеличивает версию — идущая перестройка при переключении получит `Conflict` и начнётся заново.

use async_trait::async_trait;
use sqlx::FromRow;
use sqlx::types::Json;
use time::OffsetDateTime;
use tp_contracts::meta::AccountScope;
use tp_contracts::search::{
    IndexManifest, IndexManifestStore, IndexSegment, ManifestUpdate, RebuildReason, RebuildRequest,
};
use tp_contracts::{AccountId, Error, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin, begin_system, db_err, to_u32, to_u64};

fn reason_str(r: RebuildReason) -> &'static str {
    match r {
        RebuildReason::Admin => "admin",
        RebuildReason::Corrupt => "corrupt",
        RebuildReason::Restore => "restore",
    }
}

fn parse_reason(s: &str) -> Result<RebuildReason> {
    match s {
        "admin" => Ok(RebuildReason::Admin),
        "corrupt" => Ok(RebuildReason::Corrupt),
        "restore" => Ok(RebuildReason::Restore),
        other => Err(Error::internal(format!(
            "index_manifests.rebuild_reason: {other}"
        ))),
    }
}

#[derive(FromRow)]
struct ManifestRow {
    version: i64,
    schema_version: i64,
    segments: Json<Vec<IndexSegment>>,
    rebuild_requested_at: Option<OffsetDateTime>,
    rebuild_reason: Option<String>,
}

impl TryFrom<ManifestRow> for IndexManifest {
    type Error = Error;

    fn try_from(r: ManifestRow) -> Result<Self> {
        let rebuild = match (r.rebuild_requested_at, r.rebuild_reason) {
            (Some(requested_at), Some(reason)) => Some(RebuildRequest {
                requested_at,
                reason: parse_reason(&reason)?,
            }),
            _ => None,
        };
        Ok(IndexManifest {
            version: to_u64(r.version)?,
            schema: to_u32(r.schema_version)?,
            segments: r.segments.0,
            rebuild,
        })
    }
}

fn expected_arg(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| Error::Conflict)
}

#[async_trait]
impl IndexManifestStore for PgMeta {
    async fn get_manifest(&self, scope: AccountScope) -> Result<IndexManifest> {
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let row: Option<ManifestRow> = sqlx::query_as(
            "SELECT version, schema_version, segments, rebuild_requested_at, rebuild_reason
             FROM index_manifests WHERE tenant_id = $1 AND account_id = $2",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        row.map_or(Ok(IndexManifest::default()), IndexManifest::try_from)
    }

    async fn put_manifest(
        &self,
        scope: AccountScope,
        expected: u64,
        update: ManifestUpdate,
    ) -> Result<u64> {
        let segments = Json(&update.segments);
        let schema = i64::from(update.schema);
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let version: Option<i64> = if expected == 0 {
            // Индекса ещё нет: вставка; строка уже есть (запись или запрос перестройки) — версия не 0.
            sqlx::query_scalar(
                "INSERT INTO index_manifests (tenant_id, account_id, version, schema_version, segments)
                 VALUES ($1, $2, 1, $3, $4)
                 ON CONFLICT (tenant_id, account_id) DO NOTHING
                 RETURNING version",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(schema)
            .bind(segments)
            .fetch_optional(&mut *tx)
            .await
        } else {
            sqlx::query_scalar(
                "UPDATE index_manifests SET version = version + 1, schema_version = $4, segments = $5,
                        rebuild_requested_at = CASE WHEN $6 THEN NULL ELSE rebuild_requested_at END,
                        rebuild_reason = CASE WHEN $6 THEN NULL ELSE rebuild_reason END,
                        updated_at = now()
                 WHERE tenant_id = $1 AND account_id = $2 AND version = $3
                 RETURNING version",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(expected_arg(expected)?)
            .bind(schema)
            .bind(segments)
            .bind(update.rebuilt)
            .fetch_optional(&mut *tx)
            .await
        }
        .map_err(db_err)?;
        let version = version.ok_or(Error::Conflict)?;
        tx.commit().await.map_err(db_err)?;
        to_u64(version)
    }

    async fn request_rebuild(&self, scope: AccountScope, request: RebuildRequest) -> Result<u64> {
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let version: i64 = sqlx::query_scalar(
            "INSERT INTO index_manifests (tenant_id, account_id, version, schema_version,
                                          rebuild_requested_at, rebuild_reason)
             VALUES ($1, $2, 1, 0, $3, $4)
             ON CONFLICT (tenant_id, account_id) DO UPDATE SET
                 version = index_manifests.version + 1,
                 rebuild_requested_at = EXCLUDED.rebuild_requested_at,
                 rebuild_reason = EXCLUDED.rebuild_reason,
                 updated_at = now()
             RETURNING version",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(request.requested_at)
        .bind(reason_str(request.reason))
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        to_u64(version)
    }

    async fn pending_rebuilds(&self, schema: u32, limit: usize) -> Result<Vec<AccountScope>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut tx = begin_system(&self.pool).await?;
        let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT tenant_id, account_id FROM index_manifests
             WHERE rebuild_requested_at IS NOT NULL OR schema_version < $1
             ORDER BY rebuild_requested_at NULLS LAST, updated_at, tenant_id, account_id
             LIMIT $2",
        )
        .bind(i64::from(schema))
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(t, a)| AccountScope {
                tenant: TenantId::from_uuid(t),
                account: AccountId::from_uuid(a),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_roundtrip() {
        for r in [
            RebuildReason::Admin,
            RebuildReason::Corrupt,
            RebuildReason::Restore,
        ] {
            assert_eq!(parse_reason(reason_str(r)).expect("reason"), r);
        }
        assert!(parse_reason("x").is_err());
        assert_eq!(expected_arg(u64::MAX), Err(Error::Conflict));
    }
}
