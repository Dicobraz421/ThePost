//! Корзина, квоты и политики на PostgreSQL (CONTRACTS.md#Trash, Retention, Quota (Ф1); SRS 4.7, 3.8).
//!
//! Элемент корзины — письмо с `trashed_at` в таблице `messages`: перенос в корзину (`MessageStore::to_trash`,
//! `MailboxStore::delete` с `cascade`) запоминает исходную папку, момент и срок окончательного удаления
//! `trash_expires_at`. Срок вычисляет СУБД при переносе: самый специфичный уровень политики
//! (`tp_trash_retention_us`: папка → учётная запись → домен → организация), иначе
//! [`RetentionConfig::trash_retention`]. Изменение политики действует на элементы, удалённые после него.
//!
//! Политики — таблица `policies`, одна строка на уровень цепочки ([`PolicyTarget`]); [`PolicyResolver`]
//! накладывает уровни по порядку ([`Policy::overlay`]).

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection};
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::ext::DecisionHooks;
use tp_contracts::meta::{AccountScope, MessageStore, Policy, PolicyResolver, QuotaUsage};
use tp_contracts::retention::{
    PurgeOutcome, Quota, Restored, TrashItem, TrashObject, TrashPurge, TrashPurgeSetting,
    TrashStore,
};
use tp_contracts::{AccountId, DomainId, Error, MailboxId, MessageId, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin, begin_system, db_err, to_i64, to_u32, to_u64};

/// Значения по умолчанию политик хранения (секция `meta.retention`): действуют, когда уровень
/// цепочки политик их не задаёт.
///
/// ```yaml
/// meta:
///   retention:
///     trash_retention: 30d
///     quota_bytes: 10737418240
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// Срок хранения в корзине.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub trash_retention: Duration,
    /// Квота учётной записи, байт; `None` — без ограничения.
    pub quota_bytes: Option<u64>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            trash_retention: Duration::from_secs(30 * 24 * 3600),
            quota_bytes: None,
        }
    }
}

/// Уровень цепочки политик.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyTarget {
    /// Организация.
    Tenant,
    /// Домен.
    Domain(DomainId),
    /// Учётная запись (ящик).
    Account(AccountId),
    /// Папка.
    Mailbox(MailboxId),
}

impl PolicyTarget {
    fn level(self) -> &'static str {
        match self {
            PolicyTarget::Tenant => "tenant",
            PolicyTarget::Domain(_) => "domain",
            PolicyTarget::Account(_) => "account",
            PolicyTarget::Mailbox(_) => "mailbox",
        }
    }

    fn object(self, tenant: TenantId) -> Uuid {
        match self {
            PolicyTarget::Tenant => *tenant.as_uuid(),
            PolicyTarget::Domain(id) => *id.as_uuid(),
            PolicyTarget::Account(id) => *id.as_uuid(),
            PolicyTarget::Mailbox(id) => *id.as_uuid(),
        }
    }
}

/// Длительность → микросекунды (`bigint`).
pub(crate) fn micros(d: Duration) -> Result<i64> {
    i64::try_from(d.as_micros()).map_err(|_| Error::invalid("срок слишком велик"))
}

fn from_micros(v: i64) -> Result<Duration> {
    to_u64(v).map(Duration::from_micros)
}

#[derive(FromRow)]
struct PolicyRow {
    level: String,
    quota_bytes: Option<i64>,
    trash_retention_us: Option<i64>,
    retention_us: Option<i64>,
    send_per_hour: Option<i64>,
    max_recipients: Option<i64>,
    dkim_selector: Option<String>,
    milter_profile: Option<String>,
}

impl PolicyRow {
    fn rank(&self) -> u8 {
        match self.level.as_str() {
            "tenant" => 0,
            "domain" => 1,
            "account" => 2,
            _ => 3,
        }
    }

    fn into_policy(self) -> Result<Policy> {
        Ok(Policy {
            quota_bytes: self.quota_bytes.map(to_u64).transpose()?,
            trash_retention: self.trash_retention_us.map(from_micros).transpose()?,
            retention: self.retention_us.map(from_micros).transpose()?,
            send_per_hour: self.send_per_hour.map(to_u32).transpose()?,
            max_recipients: self.max_recipients.map(to_u32).transpose()?,
            dkim_selector: self.dkim_selector,
            milter_profile: self.milter_profile,
            // Хранение правила `trash.purge` — META-29.
            trash_purge: None,
        })
    }
}

const POLICY_COLUMNS: &str = "level, quota_bytes, trash_retention_us, retention_us, send_per_hour, \
                              max_recipients, dkim_selector, milter_profile";

/// Действующая политика в открытой транзакции организации: заданные уровни по порядку цепочки.
async fn resolve_in(
    tx: &mut PgConnection,
    tenant: TenantId,
    domain: Option<DomainId>,
    account: Option<AccountId>,
    mailbox: Option<MailboxId>,
) -> Result<Policy> {
    let mut rows: Vec<PolicyRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {POLICY_COLUMNS} FROM policies
         WHERE tenant_id = $1 AND ((level = 'tenant' AND object_id = $1) OR (level = 'domain' AND object_id = $2)
                                OR (level = 'account' AND object_id = $3) OR (level = 'mailbox' AND object_id = $4))"
    )))
    .bind(tenant.as_uuid())
    .bind(domain.map(|d| *d.as_uuid()))
    .bind(account.map(|a| *a.as_uuid()))
    .bind(mailbox.map(|m| *m.as_uuid()))
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    rows.sort_by_key(PolicyRow::rank);
    rows.into_iter().try_fold(
        Policy::default(),
        |acc, r| Ok(acc.overlay(r.into_policy()?)),
    )
}

impl PgMeta {
    /// Политика одного уровня цепочки (без наложения); не задана — все поля `None`.
    pub async fn policy(&self, tenant: TenantId, target: PolicyTarget) -> Result<Policy> {
        let mut tx = begin(&self.pool, tenant).await?;
        let row: Option<PolicyRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {POLICY_COLUMNS} FROM policies WHERE tenant_id = $1 AND level = $2 AND object_id = $3"
        )))
        .bind(tenant.as_uuid())
        .bind(target.level())
        .bind(target.object(tenant))
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        row.map_or(Ok(Policy::default()), PolicyRow::into_policy)
    }

    /// Задаёт политику уровня целиком (Admin API); политика без заданных полей удаляет уровень.
    pub async fn set_policy(
        &self,
        tenant: TenantId,
        target: PolicyTarget,
        policy: &Policy,
    ) -> Result<()> {
        let mut tx = begin(&self.pool, tenant).await?;
        if *policy == Policy::default() {
            sqlx::query(
                "DELETE FROM policies WHERE tenant_id = $1 AND level = $2 AND object_id = $3",
            )
            .bind(tenant.as_uuid())
            .bind(target.level())
            .bind(target.object(tenant))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        } else {
            sqlx::query(
                "INSERT INTO policies (tenant_id, level, object_id, quota_bytes, trash_retention_us, retention_us,
                                       send_per_hour, max_recipients, dkim_selector, milter_profile)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT (tenant_id, level, object_id) DO UPDATE SET
                     quota_bytes = EXCLUDED.quota_bytes, trash_retention_us = EXCLUDED.trash_retention_us,
                     retention_us = EXCLUDED.retention_us, send_per_hour = EXCLUDED.send_per_hour,
                     max_recipients = EXCLUDED.max_recipients, dkim_selector = EXCLUDED.dkim_selector,
                     milter_profile = EXCLUDED.milter_profile, updated_at = now()",
            )
            .bind(tenant.as_uuid())
            .bind(target.level())
            .bind(target.object(tenant))
            .bind(policy.quota_bytes.map(to_i64).transpose()?)
            .bind(policy.trash_retention.map(micros).transpose()?)
            .bind(policy.retention.map(micros).transpose()?)
            .bind(policy.send_per_hour.map(i64::from))
            .bind(policy.max_recipients.map(i64::from))
            .bind(&policy.dkim_selector)
            .bind(&policy.milter_profile)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }
}

#[async_trait]
impl PolicyResolver for PgMeta {
    /// Уровни, переданные как `None`, не участвуют; заданные накладываются по порядку цепочки.
    async fn resolve(
        &self,
        tenant: TenantId,
        domain: Option<DomainId>,
        account: Option<AccountId>,
        mailbox: Option<MailboxId>,
    ) -> Result<Policy> {
        let mut tx = begin(&self.pool, tenant).await?;
        let p = resolve_in(&mut tx, tenant, domain, account, mailbox).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(p)
    }

    // Настройка `trash.purge` на PostgreSQL — META-29 (CTR-26 добавил методы контракта).
    async fn trash_purge(&self, _scope: AccountScope) -> Result<TrashPurgeSetting> {
        Err(Error::internal("trash.purge: не реализовано (META-29)"))
    }

    async fn set_user_trash_purge(
        &self,
        _scope: AccountScope,
        _mode: Option<TrashPurge>,
    ) -> Result<()> {
        Err(Error::internal("trash.purge: не реализовано (META-29)"))
    }
}

// ---------- Корзина ----------

#[derive(FromRow)]
struct TrashRow {
    tenant_id: Uuid,
    account_id: Uuid,
    message_id: Uuid,
    trashed_from: Option<Uuid>,
    size: i64,
    received_at: OffsetDateTime,
    trashed_at: OffsetDateTime,
    trash_expires_at: Option<OffsetDateTime>,
}

const TRASH_COLUMNS: &str = "tenant_id, account_id, message_id, trashed_from, size, received_at, trashed_at, trash_expires_at";

impl TryFrom<TrashRow> for TrashItem {
    type Error = Error;

    fn try_from(r: TrashRow) -> Result<Self> {
        Ok(TrashItem {
            scope: AccountScope {
                tenant: TenantId::from_uuid(r.tenant_id),
                account: AccountId::from_uuid(r.account_id),
            },
            object: TrashObject::Message(MessageId::from_uuid(r.message_id)),
            origin: MailboxId::from_uuid(
                r.trashed_from
                    .ok_or_else(|| Error::internal("письмо в корзине без исходной папки"))?,
            ),
            size: to_u64(r.size)?,
            received_at: r.received_at,
            deleted_at: r.trashed_at,
            expires_at: r
                .trash_expires_at
                .ok_or_else(|| Error::internal("письмо в корзине без срока"))?,
        })
    }
}

fn message_of(object: TrashObject) -> Result<MessageId> {
    match object {
        TrashObject::Message(id) => Ok(id),
        #[allow(unreachable_patterns)]
        _ => Err(Error::invalid("объект корзины не поддерживается")),
    }
}

fn limit_arg(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

#[async_trait]
impl TrashStore for PgMeta {
    async fn list(
        &self,
        scope: AccountScope,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<TrashItem>> {
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let rows: Vec<TrashRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {TRASH_COLUMNS} FROM messages
             WHERE tenant_id = $1 AND account_id = $2 AND trashed_at IS NOT NULL
             ORDER BY trashed_at DESC, message_id DESC OFFSET $3 LIMIT $4"
        )))
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(limit_arg(offset))
        .bind(limit_arg(limit))
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        rows.into_iter().map(TrashItem::try_from).collect()
    }

    // Скрытые письма и «очистить корзину» на PostgreSQL — META-29 (CTR-26 добавил методы контракта).
    async fn list_hidden(
        &self,
        _scope: AccountScope,
        _offset: usize,
        _limit: usize,
    ) -> Result<Vec<TrashItem>> {
        Err(Error::internal("trash.purge: не реализовано (META-29)"))
    }

    async fn purge(
        &self,
        _scope: AccountScope,
        _ids: &[MessageId],
        _hooks: &DecisionHooks,
    ) -> Result<PurgeOutcome> {
        Err(Error::internal("trash.purge: не реализовано (META-29)"))
    }

    async fn restore(&self, scope: AccountScope, object: TrashObject) -> Result<Restored> {
        let id = message_of(object)?;
        // Письмо не в корзине (в том числе восстановлено параллельно) — элемента нет.
        let inserted = match MessageStore::restore(self, scope, &[id]).await {
            Err(Error::Invalid(_)) => return Err(Error::NotFound),
            r => r?.into_iter().next().ok_or(Error::NotFound)?,
        };
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let mailbox: Option<Uuid> = sqlx::query_scalar(
            "SELECT mailbox_id FROM messages WHERE tenant_id = $1 AND account_id = $2 AND message_id = $3",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(Restored::Message {
            mailbox: MailboxId::from_uuid(mailbox.ok_or(Error::NotFound)?),
            inserted,
        })
    }

    async fn expired(&self, now: OffsetDateTime, limit: usize) -> Result<Vec<TrashItem>> {
        let mut tx = begin_system(&self.pool).await?;
        let rows: Vec<TrashRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {TRASH_COLUMNS} FROM messages
             WHERE trash_expires_at <= $1
             ORDER BY trash_expires_at, tenant_id, account_id, message_id LIMIT $2"
        )))
        .bind(now)
        .bind(limit_arg(limit))
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        rows.into_iter().map(TrashItem::try_from).collect()
    }

    async fn defer(
        &self,
        scope: AccountScope,
        object: TrashObject,
        until: OffsetDateTime,
    ) -> Result<()> {
        let id = message_of(object)?;
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let n = sqlx::query(
            "UPDATE messages SET trash_expires_at = $4
             WHERE tenant_id = $1 AND account_id = $2 AND message_id = $3 AND trashed_at IS NOT NULL",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(id.as_uuid())
        .bind(until)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        tx.commit().await.map_err(db_err)?;
        if n == 1 { Ok(()) } else { Err(Error::NotFound) }
    }
}

// ---------- Квота ----------

#[async_trait]
impl Quota for PgMeta {
    /// Использование — сумма размеров писем учётной записи, включая корзину; лимит — политика
    /// учётной записи по цепочке, иначе [`RetentionConfig::quota_bytes`].
    async fn usage(&self, scope: AccountScope) -> Result<QuotaUsage> {
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let row: Option<(Uuid, i64, i64)> = sqlx::query_as(
            "SELECT a.domain_id, coalesce(sum(m.size), 0)::bigint, count(m.message_id)
             FROM accounts a
             LEFT JOIN messages m ON m.tenant_id = a.tenant_id AND m.account_id = a.account_id
             WHERE a.tenant_id = $1 AND a.account_id = $2
             GROUP BY a.domain_id",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        let (domain, used, messages) = row.ok_or(Error::NotFound)?;
        let policy = resolve_in(
            &mut tx,
            scope.tenant,
            Some(DomainId::from_uuid(domain)),
            Some(scope.account),
            None,
        )
        .await?;
        tx.commit().await.map_err(db_err)?;
        Ok(QuotaUsage {
            used_bytes: to_u64(used)?,
            messages: to_u64(messages)?,
            limit_bytes: policy.quota_bytes.or(self.cfg.retention.quota_bytes),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_yaml() {
        let c: RetentionConfig =
            serde_yaml_ng::from_str("trash_retention: 7d\nquota_bytes: 1000\n").expect("yaml");
        assert_eq!(c.trash_retention, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(c.quota_bytes, Some(1000));
        let d: RetentionConfig = serde_yaml_ng::from_str("{}").expect("yaml");
        assert_eq!(d, RetentionConfig::default());
        assert!(serde_yaml_ng::from_str::<RetentionConfig>("trash: 1d").is_err());
    }

    #[test]
    fn micros_roundtrip() {
        let d = Duration::from_secs(30 * 24 * 3600);
        assert_eq!(from_micros(micros(d).expect("us")).expect("d"), d);
        assert!(micros(Duration::MAX).is_err());
        assert!(from_micros(-1).is_err());
    }

    #[test]
    fn levels_are_ranked_in_chain_order() {
        let t = TenantId::new();
        let row = |level: &str| PolicyRow {
            level: level.into(),
            quota_bytes: None,
            trash_retention_us: None,
            retention_us: None,
            send_per_hour: None,
            max_recipients: None,
            dkim_selector: None,
            milter_profile: None,
        };
        let ranks: Vec<u8> = ["tenant", "domain", "account", "mailbox"]
            .iter()
            .map(|l| row(l).rank())
            .collect();
        assert_eq!(ranks, [0, 1, 2, 3]);
        assert_eq!(PolicyTarget::Tenant.object(t), *t.as_uuid());
    }
}
