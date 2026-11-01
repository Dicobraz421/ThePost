//! Учётные записи на PostgreSQL (CONTRACTS.md#Metadata).

use async_trait::async_trait;
use sqlx::{FromRow, PgConnection};
use tp_contracts::events::CoreEventKind;
use tp_contracts::meta::{
    Account, AccountScope, AccountStore, AccountUpsert, NewMailbox, QuotaUsage,
};
use tp_contracts::{AccountId, DomainId, Error, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::changelog::{self, Change, Op};
use crate::mailboxes::{self, SYSTEM_FOLDERS};
use crate::schema::{begin, db_err, to_u64};

#[derive(FromRow)]
struct AccountRow {
    account_id: Uuid,
    tenant_id: Uuid,
    domain_id: Uuid,
    login: String,
    display_name: String,
    external_id: Option<String>,
    enabled: bool,
}

impl From<AccountRow> for Account {
    fn from(r: AccountRow) -> Self {
        Account {
            id: AccountId::from_uuid(r.account_id),
            tenant: TenantId::from_uuid(r.tenant_id),
            domain: DomainId::from_uuid(r.domain_id),
            login: r.login,
            display_name: r.display_name,
            external_id: r.external_id,
            enabled: r.enabled,
        }
    }
}

/// Сопоставление при синхронизации: по `external_id`, иначе по имени входа.
/// Запись с тем же именем, но другим `external_id` — `Error::Conflict`.
async fn find_for_upsert(
    tx: &mut PgConnection,
    tenant: TenantId,
    data: &AccountUpsert,
) -> Result<Option<Uuid>> {
    if let Some(ext) = &data.external_id {
        let id: Option<Uuid> = sqlx::query_scalar(
            "SELECT account_id FROM accounts WHERE tenant_id = $1 AND external_id = $2 FOR UPDATE",
        )
        .bind(tenant.as_uuid())
        .bind(ext)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        if id.is_some() {
            return Ok(id);
        }
    }
    let row: Option<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT account_id, external_id FROM accounts WHERE tenant_id = $1 AND lower(login) = lower($2) FOR UPDATE",
    )
    .bind(tenant.as_uuid())
    .bind(&data.login)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    match row {
        Some((_, Some(other))) if data.external_id.as_ref().is_some_and(|ext| *ext != other) => {
            Err(Error::Conflict)
        }
        Some((id, _)) => Ok(Some(id)),
        None => Ok(None),
    }
}

impl PgMeta {
    async fn upsert_once(&self, tenant: TenantId, data: &AccountUpsert) -> Result<Account> {
        if data.login.is_empty() {
            return Err(Error::invalid("пустое имя входа"));
        }
        let mut tx = begin(&self.pool, tenant).await?;
        let account: AccountRow = match find_for_upsert(&mut tx, tenant, data).await? {
            Some(id) => sqlx::query_as(
                "UPDATE accounts SET domain_id = $3, login = $4, display_name = $5, external_id = $6,
                                     enabled = $7, updated_at = now()
                 WHERE tenant_id = $1 AND account_id = $2
                 RETURNING account_id, tenant_id, domain_id, login, display_name, external_id, enabled",
            )
            .bind(tenant.as_uuid())
            .bind(id),
            None => sqlx::query_as(
                "INSERT INTO accounts (tenant_id, account_id, domain_id, login, display_name, external_id, enabled)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 RETURNING account_id, tenant_id, domain_id, login, display_name, external_id, enabled",
            )
            .bind(tenant.as_uuid())
            .bind(*AccountId::new().as_uuid()),
        }
        .bind(data.domain.as_uuid())
        .bind(&data.login)
        .bind(&data.display_name)
        .bind(&data.external_id)
        .bind(data.enabled)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let account = Account::from(account);

        // Новая учётная запись: системные папки.
        let scope = AccountScope {
            tenant,
            account: account.id,
        };
        let has_folders: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM mailboxes WHERE tenant_id = $1 AND account_id = $2)",
        )
        .bind(tenant.as_uuid())
        .bind(account.id.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        if !has_folders {
            let ms = changelog::bump_modseq(&mut tx, scope).await?;
            let mut changes = Vec::with_capacity(SYSTEM_FOLDERS.len());
            let mut events = Vec::with_capacity(SYSTEM_FOLDERS.len());
            for (name, role) in SYSTEM_FOLDERS {
                let data = NewMailbox {
                    parent: None,
                    name: name.to_owned(),
                    role: Some(role),
                };
                let mb = mailboxes::insert(&mut tx, scope, &data, ms).await?;
                changes.push(Change::mailbox(mb.id, Op::Created));
                events.push(changelog::event(
                    scope,
                    Some(mb.id),
                    CoreEventKind::MailboxCreated,
                ));
            }
            changelog::log(&mut tx, scope, ms, &changes).await?;
            changelog::append_events(&mut tx, self.cfg.event_partitions, &events).await?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(account)
    }
}

#[async_trait]
impl AccountStore for PgMeta {
    async fn get(&self, tenant: TenantId, id: AccountId) -> Result<Option<Account>> {
        let mut tx = begin(&self.pool, tenant).await?;
        let row: Option<AccountRow> = sqlx::query_as(
            "SELECT account_id, tenant_id, domain_id, login, display_name, external_id, enabled
             FROM accounts WHERE tenant_id = $1 AND account_id = $2",
        )
        .bind(tenant.as_uuid())
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(row.map(Account::from))
    }

    async fn by_login(&self, tenant: TenantId, login: &str) -> Result<Option<Account>> {
        let mut tx = begin(&self.pool, tenant).await?;
        let row: Option<AccountRow> = sqlx::query_as(
            "SELECT account_id, tenant_id, domain_id, login, display_name, external_id, enabled
             FROM accounts WHERE tenant_id = $1 AND lower(login) = lower($2)",
        )
        .bind(tenant.as_uuid())
        .bind(login)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(row.map(Account::from))
    }

    async fn upsert(&self, tenant: TenantId, data: AccountUpsert) -> Result<Account> {
        // Параллельное создание той же учётной записи: проигравший повторяет как обновление.
        match self.upsert_once(tenant, &data).await {
            Err(Error::Conflict) => self.upsert_once(tenant, &data).await,
            r => r,
        }
    }

    /// Лимит (`limit_bytes`) разрешает `PolicyResolver`; здесь — только использование.
    async fn quota(&self, tenant: TenantId, id: AccountId) -> Result<QuotaUsage> {
        let mut tx = begin(&self.pool, tenant).await?;
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT coalesce(sum(m.size), 0)::bigint, count(m.message_id)
             FROM accounts a
             LEFT JOIN messages m ON m.tenant_id = a.tenant_id AND m.account_id = a.account_id
                                 AND m.hidden_at IS NULL
             WHERE a.tenant_id = $1 AND a.account_id = $2
             GROUP BY a.account_id",
        )
        .bind(tenant.as_uuid())
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        let (used, messages) = row.ok_or(Error::NotFound)?;
        Ok(QuotaUsage {
            used_bytes: to_u64(used)?,
            messages: to_u64(messages)?,
            limit_bytes: None,
        })
    }
}
