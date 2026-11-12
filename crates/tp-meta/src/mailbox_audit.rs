//! Аудит доступа к ящику на PostgreSQL (x/jmap-08; CONTRACTS.md#MailboxAudit; CTR-35; SRS 5.2).
//!
//! Журнал `mailbox_audit` — в основной БД, только на добавление (UPDATE и DELETE отклоняет триггер), секционирован
//! по суткам UTC, как журнал доставки: секции создаются перед записью, устаревшие удаляются целиком
//! ([`MailboxAudit::purge_audit`], срок — [`MailboxAuditConfig::retention`], [`PgMeta::purge_expired_mailbox_audit`]).
//! Политики организации и ящика — `mailbox_audit_policies`; запись применяет политику в той же транзакции.
//! Позиция страницы — [`crate::delivery::position`] последней записи страницы.

use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection};
use time::{OffsetDateTime, UtcOffset};
use tp_contracts::audit::{
    AuditAction, AuditCursor, AuditEntry, AuditObject, AuditPage, AuditPolicy, AuditQuery,
    MailboxAudit,
};
use tp_contracts::config::duration;
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Error, MailboxId, MessageId, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::delivery::{day_partitions, parse_position, position, purge_days};
use crate::pool::with_tenant;
use crate::schema::db_err;

/// Аудит ящика: секция `meta.mailbox_audit` (SRS 5.2, 5.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MailboxAuditConfig {
    /// Срок хранения записей; удаление — сутками ([`PgMeta::purge_expired_mailbox_audit`]).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retention: Duration,
}

impl Default for MailboxAuditConfig {
    fn default() -> Self {
        Self {
            retention: Duration::from_secs(90 * 24 * 3600),
        }
    }
}

/// Значение перечисления в виде строки serde (`owner`, `send_as`) — так оно хранится в БД.
fn enum_str<T: Serialize>(v: T) -> Result<String> {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        _ => Err(Error::internal("аудит ящика: значение перечисления")),
    }
}

fn enum_of<T: DeserializeOwned>(s: String) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s))
        .map_err(|e| Error::internal(format!("аудит ящика: {e}")))
}

#[derive(FromRow)]
struct EntryRow {
    account_id: Uuid,
    at: OffsetDateTime,
    seq: i64,
    actor: String,
    actor_account: Option<Uuid>,
    actor_kind: String,
    action: String,
    mailbox_id: Option<Uuid>,
    message_id: Option<Uuid>,
    detail: Option<String>,
    client: Option<String>,
}

impl EntryRow {
    fn into_entry(self, tenant: TenantId) -> Result<AuditEntry> {
        Ok(AuditEntry {
            scope: AccountScope {
                tenant,
                account: AccountId::from_uuid(self.account_id),
            },
            at: self.at,
            actor: self.actor,
            actor_account: self.actor_account.map(AccountId::from_uuid),
            actor_kind: enum_of(self.actor_kind)?,
            action: enum_of(self.action)?,
            object: AuditObject {
                mailbox: self.mailbox_id.map(MailboxId::from_uuid),
                message: self.message_id.map(MessageId::from_uuid),
                detail: self.detail,
            },
            client: self.client,
        })
    }
}

/// Действующая политика ящика: своя, иначе организации, иначе по умолчанию.
async fn policy(tx: &mut PgConnection, scope: AccountScope) -> Result<AuditPolicy> {
    let row: Option<(bool, Vec<String>)> = sqlx::query_as(
        "SELECT non_owner, owner_actions FROM mailbox_audit_policies
         WHERE tenant_id = $1 AND (account_id = $2 OR account_id IS NULL)
         ORDER BY account_id NULLS LAST LIMIT 1",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    let Some((non_owner, owner)) = row else {
        return Ok(AuditPolicy::default());
    };
    Ok(AuditPolicy {
        non_owner,
        owner: owner
            .into_iter()
            .map(enum_of::<AuditAction>)
            .collect::<Result<BTreeSet<_>>>()?,
    })
}

impl PgMeta {
    /// Удаляет записи аудита ящиков старше срока хранения ([`MailboxAuditConfig::retention`]) — роль maintenance.
    pub async fn purge_expired_mailbox_audit(&self) -> Result<()> {
        let retention = time::Duration::try_from(self.cfg.mailbox_audit.retention)
            .map_err(|_| Error::invalid("meta.mailbox_audit.retention вне диапазона"))?;
        let before = OffsetDateTime::now_utc()
            .checked_sub(retention)
            .ok_or_else(|| Error::invalid("meta.mailbox_audit.retention вне диапазона"))?;
        self.purge_audit(before).await
    }
}

#[async_trait]
impl MailboxAudit for PgMeta {
    /// Запись без ключа: выполняется один раз, временный отказ отдаётся вызывающему.
    async fn record_audit(&self, entry: AuditEntry) -> Result<bool> {
        entry.validate()?;
        let kind = enum_str(entry.actor_kind)?;
        let action = enum_str(entry.action)?;
        let day = entry.at.to_offset(UtcOffset::UTC).date();
        let tenant = entry.scope.tenant;
        self.once(with_tenant(&self.pool, tenant, move |tx| {
            Box::pin(async move {
                if !policy(tx, entry.scope)
                    .await?
                    .records(entry.actor_kind, entry.action)
                {
                    return Ok(false);
                }
                day_partitions(tx, "mailbox_audit", &[day]).await?;
                sqlx::query(
                    "INSERT INTO mailbox_audit (tenant_id, account_id, at, actor, actor_key, actor_account,
                                                actor_kind, action, mailbox_id, message_id, detail, client)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                )
                .bind(tenant.as_uuid())
                .bind(entry.scope.account.as_uuid())
                .bind(entry.at)
                .bind(&entry.actor)
                .bind(entry.actor.to_ascii_lowercase())
                .bind(entry.actor_account.map(|a| *a.as_uuid()))
                .bind(&kind)
                .bind(&action)
                .bind(entry.object.mailbox.map(|m| *m.as_uuid()))
                .bind(entry.object.message.map(|m| *m.as_uuid()))
                .bind(&entry.object.detail)
                .bind(&entry.client)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                Ok(true)
            })
        }))
        .await
    }

    async fn audit_policy(&self, scope: AccountScope) -> Result<AuditPolicy> {
        self.idempotent(|| {
            with_tenant(&self.pool, scope.tenant, move |tx| {
                Box::pin(async move { policy(tx, scope).await })
            })
        })
        .await
    }

    /// Задание и сброс по значению — идемпотентны по сути.
    async fn set_audit_policy(
        &self,
        tenant: TenantId,
        account: Option<AccountId>,
        policy: Option<AuditPolicy>,
    ) -> Result<()> {
        let account = account.map(|a| *a.as_uuid());
        let policy = match policy {
            Some(p) => {
                p.validate()?;
                let owner = p
                    .owner
                    .iter()
                    .map(|a| enum_str(*a))
                    .collect::<Result<Vec<_>>>()?;
                Some((p.non_owner, owner))
            }
            None => None,
        };
        self.rewrite(|| {
            let policy = policy.clone();
            with_tenant(&self.pool, tenant, move |tx| {
                Box::pin(async move {
                    match policy {
                        Some((non_owner, owner)) => {
                            sqlx::query(
                                "INSERT INTO mailbox_audit_policies (tenant_id, account_id, non_owner, owner_actions)
                                 VALUES ($1, $2, $3, $4)
                                 ON CONFLICT (tenant_id, account_id) DO UPDATE
                                     SET non_owner = EXCLUDED.non_owner, owner_actions = EXCLUDED.owner_actions",
                            )
                            .bind(tenant.as_uuid())
                            .bind(account)
                            .bind(non_owner)
                            .bind(owner)
                            .execute(&mut *tx)
                            .await
                            .map_err(db_err)?;
                        }
                        None => {
                            sqlx::query(
                                "DELETE FROM mailbox_audit_policies
                                 WHERE tenant_id = $1 AND account_id IS NOT DISTINCT FROM $2",
                            )
                            .bind(tenant.as_uuid())
                            .bind(account)
                            .execute(&mut *tx)
                            .await
                            .map_err(db_err)?;
                        }
                    }
                    Ok(())
                })
            })
        })
        .await
    }

    async fn search_audit(&self, tenant: TenantId, query: &AuditQuery) -> Result<AuditPage> {
        query.validate()?;
        let after = query
            .after
            .as_ref()
            .map(|c| parse_position(&c.0))
            .transpose()?;
        let limit = i64::try_from(query.limit).unwrap_or(i64::MAX - 1);
        let account = query.account.map(|a| *a.as_uuid());
        let actor = query.actor.as_deref().map(str::to_ascii_lowercase);
        let kind = query.actor_kind.map(enum_str).transpose()?;
        let action = query.action.map(enum_str).transpose()?;
        let (since, until) = (query.since, query.until);
        let mut rows: Vec<EntryRow> = self
            .idempotent(|| {
                let (actor, kind, action) = (actor.clone(), kind.clone(), action.clone());
                with_tenant(&self.pool, tenant, move |tx| {
                    Box::pin(async move {
                        sqlx::query_as(
                            "SELECT account_id, at, seq, actor, actor_account, actor_kind, action, mailbox_id,
                                    message_id, detail, client
                             FROM mailbox_audit
                             WHERE tenant_id = $1
                               AND ($2::uuid IS NULL OR account_id = $2)
                               AND ($3::text IS NULL OR actor_key = $3)
                               AND ($4::text IS NULL OR actor_kind = $4)
                               AND ($5::text IS NULL OR action = $5)
                               AND ($6::timestamptz IS NULL OR at >= $6)
                               AND ($7::timestamptz IS NULL OR at < $7)
                               AND ($8::timestamptz IS NULL OR (at, seq) > ($8, $9::bigint))
                             ORDER BY at, seq LIMIT $10",
                        )
                        .bind(tenant.as_uuid())
                        .bind(account)
                        .bind(actor)
                        .bind(kind)
                        .bind(action)
                        .bind(since)
                        .bind(until)
                        .bind(after.map(|(at, _)| at))
                        .bind(after.map(|(_, seq)| seq))
                        .bind(limit + 1)
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(db_err)
                    })
                })
            })
            .await?;
        let more = rows.len() > query.limit;
        rows.truncate(query.limit);
        let next = if more {
            rows.last().map(|r| AuditCursor(position(r.at, r.seq)))
        } else {
            None
        };
        Ok(AuditPage {
            entries: rows
                .into_iter()
                .map(|r| r.into_entry(tenant))
                .collect::<Result<_>>()?,
            next,
        })
    }

    async fn purge_audit(&self, before: OffsetDateTime) -> Result<()> {
        purge_days(self, "mailbox_audit", before).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_contracts::audit::AuditActorKind;

    #[test]
    fn enums_roundtrip() {
        for k in [
            AuditActorKind::Owner,
            AuditActorKind::Delegate,
            AuditActorKind::Admin,
        ] {
            assert_eq!(enum_of::<AuditActorKind>(enum_str(k).expect("str")), Ok(k));
        }
        assert_eq!(
            enum_str(AuditAction::SendOnBehalf).expect("str"),
            "send_on_behalf"
        );
        assert_eq!(
            enum_of::<AuditAction>("change_forwarding".into()),
            Ok(AuditAction::ChangeForwarding)
        );
        assert_eq!(
            enum_of::<AuditAction>("search".into()),
            Ok(AuditAction::Search)
        );
    }

    #[test]
    fn default_retention_is_90_days() {
        assert_eq!(
            MailboxAuditConfig::default().retention,
            Duration::from_secs(90 * 24 * 3600)
        );
    }
}
