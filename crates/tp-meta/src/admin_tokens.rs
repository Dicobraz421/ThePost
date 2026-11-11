//! Локальные одноразовые токены Admin API на PostgreSQL (META-43; CTR-41, CONTRACTS.md#Auth, решение #193): таблица
//! `local_admin_tokens`, только хэши значений.
//!
//! Выпуск и список — в транзакции организации (RLS). Погашение, отзыв и очистка — системной транзакцией: Bearer не
//! несёт организацию, идентификатор и хэш уникальны по инсталляции. Погашение — один `UPDATE … RETURNING` с условием
//! «не погашен, не отозван, срок не истёк»: параллельный вызов ждёт блокировку строки, перечитывает её и условию уже
//! не отвечает — успешен ровно один.

use async_trait::async_trait;
use sqlx::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;

use tp_contracts::admin_token::{LocalAdminToken, LocalAdminTokens};
use tp_contracts::{AccountId, Error, Result, TenantId};

use crate::PgMeta;
use crate::schema::{begin, begin_system, db_err};

#[derive(FromRow)]
struct Row {
    token_id: String,
    tenant_id: Uuid,
    account_id: Uuid,
    hash: String,
    created_at: OffsetDateTime,
    expires_at: OffsetDateTime,
    used_at: Option<OffsetDateTime>,
    revoked_at: Option<OffsetDateTime>,
}

impl From<Row> for LocalAdminToken {
    fn from(r: Row) -> Self {
        LocalAdminToken {
            id: r.token_id,
            tenant: TenantId::from_uuid(r.tenant_id),
            account: AccountId::from_uuid(r.account_id),
            hash: r.hash,
            created_at: r.created_at,
            expires_at: r.expires_at,
            used_at: r.used_at,
            revoked_at: r.revoked_at,
        }
    }
}

#[async_trait]
impl LocalAdminTokens for PgMeta {
    async fn issue(&self, token: LocalAdminToken) -> Result<()> {
        token.validate()?;
        if token.used_at.is_some() || token.revoked_at.is_some() {
            return Err(Error::invalid("новый токен уже погашен или отозван"));
        }
        // Повтор применённой вставки ответил бы `Conflict` — один раз.
        self.once(async {
            let mut tx = begin(&self.pool, token.tenant).await?;
            // Занятые идентификатор или хэш (в том числе другой организации) — нарушение уникальности (`Conflict`).
            sqlx::query(
                "INSERT INTO local_admin_tokens (token_id, tenant_id, account_id, hash, created_at, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(&token.id)
            .bind(token.tenant.as_uuid())
            .bind(token.account.as_uuid())
            .bind(&token.hash)
            .bind(token.created_at)
            .bind(token.expires_at)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn consume(&self, hash: &str, now: OffsetDateTime) -> Result<LocalAdminToken> {
        // Повтор погашения после обрыва во время COMMIT ответил бы `Forbidden` — один раз.
        self.once(async {
            let mut tx = begin_system(&self.pool).await?;
            let row: Option<Row> = sqlx::query_as(
                "UPDATE local_admin_tokens SET used_at = $2
                 WHERE hash = $1 AND used_at IS NULL AND revoked_at IS NULL AND expires_at > $2
                 RETURNING token_id, tenant_id, account_id, hash, created_at, expires_at, used_at, revoked_at",
            )
            .bind(hash)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            // Причина отказа (нет, погашен, отозван, истёк) не раскрывается.
            row.map(LocalAdminToken::from).ok_or(Error::Forbidden)
        })
        .await
    }

    async fn revoke(&self, id: &str, at: OffsetDateTime) -> Result<()> {
        // `COALESCE` — момент первого отзыва; повтор даёт то же состояние.
        self.rewrite(|| async move {
            let mut tx = begin_system(&self.pool).await?;
            let updated = sqlx::query(
                "UPDATE local_admin_tokens SET revoked_at = COALESCE(revoked_at, $2) WHERE token_id = $1",
            )
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
        })
        .await
    }

    async fn list(&self, tenant: TenantId) -> Result<Vec<LocalAdminToken>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            let rows: Vec<Row> = sqlx::query_as(
                "SELECT token_id, tenant_id, account_id, hash, created_at, expires_at, used_at, revoked_at
                 FROM local_admin_tokens WHERE tenant_id = $1
                 ORDER BY created_at, token_id COLLATE \"C\"",
            )
            .bind(tenant.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            Ok(rows.into_iter().map(LocalAdminToken::from).collect())
        })
        .await
    }

    async fn prune(&self, before: OffsetDateTime) -> Result<u64> {
        // Удаление по сроку: повтор удаляет оставшиеся (число — только их), состояние то же.
        self.rewrite(|| async move {
            let mut tx = begin_system(&self.pool).await?;
            let removed = sqlx::query("DELETE FROM local_admin_tokens WHERE expires_at < $1")
                .bind(before)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?
                .rows_affected();
            tx.commit().await.map_err(db_err)?;
            Ok(removed)
        })
        .await
    }
}
