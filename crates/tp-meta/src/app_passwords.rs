//! Пароли приложений на PostgreSQL (CTR-21, CONTRACTS.md#Auth; SRS 5.1): таблица `app_passwords`, только хэши.

use async_trait::async_trait;
use sqlx::FromRow;
use time::OffsetDateTime;

use tp_contracts::app_password::{AppPassword, AppPasswordStore};
use tp_contracts::meta::AccountScope;
use tp_contracts::{Error, Result};

use crate::PgMeta;
use crate::schema::{begin, db_err};

#[derive(FromRow)]
struct Row {
    password_id: String,
    label: String,
    hash: String,
    created_at: OffsetDateTime,
    revoked_at: Option<OffsetDateTime>,
}

impl From<Row> for AppPassword {
    fn from(r: Row) -> Self {
        AppPassword {
            id: r.password_id,
            label: r.label,
            hash: r.hash,
            created_at: r.created_at,
            revoked_at: r.revoked_at,
        }
    }
}

#[async_trait]
impl AppPasswordStore for PgMeta {
    async fn list(&self, scope: AccountScope) -> Result<Vec<AppPassword>> {
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT password_id, label, hash, created_at, revoked_at FROM app_passwords
             WHERE tenant_id = $1 AND account_id = $2
             ORDER BY created_at, password_id COLLATE \"C\"",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(rows.into_iter().map(AppPassword::from).collect())
    }

    async fn insert(&self, scope: AccountScope, password: AppPassword) -> Result<()> {
        password.validate()?;
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let found: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM accounts WHERE tenant_id = $1 AND account_id = $2")
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        if found.is_none() {
            return Err(Error::NotFound);
        }
        // Занятый идентификатор — нарушение первичного ключа (`Conflict`).
        sqlx::query(
            "INSERT INTO app_passwords (tenant_id, account_id, password_id, label, hash, created_at, revoked_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(&password.id)
        .bind(&password.label)
        .bind(&password.hash)
        .bind(password.created_at)
        .bind(password.revoked_at)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn revoke(&self, scope: AccountScope, id: &str, at: OffsetDateTime) -> Result<()> {
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let updated = sqlx::query(
            "UPDATE app_passwords SET revoked_at = COALESCE(revoked_at, $4)
             WHERE tenant_id = $1 AND account_id = $2 AND password_id = $3",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(id)
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        tx.commit().await.map_err(db_err)?;
        if updated == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }
}
