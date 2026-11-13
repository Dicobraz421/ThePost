//! Архив ящика на PostgreSQL (CONTRACTS.md#Archive; CTR-76).
//!
//! Ветка архива — папка с ролью `archive` и вложенные ([`crate::retention::ARCHIVE_BRANCH`]); квота архива —
//! письма ветки ([`PgMeta::usage_of`]), основная квота их не считает.
//!
//! Выборка «пора в архив» (`archive_due`): возраст `archive_after_us` действующих ящиков — по цепочке
//! организация → домен → учётная запись одним запросом к каталогу основной БД; письма — запросом к каждой БД ящиков
//! (основной и шардам справочника) со сроком `received_at + возраст`; итог — слияние по (`due`, `message`).

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::archive::{ArchiveItem, ArchiveStore};
use tp_contracts::meta::{AccountScope, QuotaUsage};
use tp_contracts::{AccountId, MailboxId, MessageId, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin_system, db_err};

/// Строка выборки: организация, учётная запись, письмо, папка, срок.
type DueRow = (Uuid, Uuid, Uuid, Uuid, OffsetDateTime);

#[async_trait]
impl ArchiveStore for PgMeta {
    async fn archive_due(&self, now: OffsetDateTime, limit: usize) -> Result<Vec<ArchiveItem>> {
        let lim = i64::try_from(limit).unwrap_or(i64::MAX);
        self.idempotent(|| async move {
            let mut tx = begin_system(&self.pool).await?;
            let ages: Vec<(Uuid, Uuid, i64)> = sqlx::query_as(
                "SELECT a.tenant_id, a.account_id,
                        coalesce(pa.archive_after_us, pd.archive_after_us, pt.archive_after_us)
                 FROM accounts a
                 LEFT JOIN policies pt ON pt.tenant_id = a.tenant_id AND pt.level = 'tenant'
                                      AND pt.object_id = a.tenant_id
                 LEFT JOIN policies pd ON pd.tenant_id = a.tenant_id AND pd.level = 'domain'
                                      AND pd.object_id = a.domain_id
                 LEFT JOIN policies pa ON pa.tenant_id = a.tenant_id AND pa.level = 'account'
                                      AND pa.object_id = a.account_id
                 WHERE a.disabled_until IS NULL
                   AND coalesce(pa.archive_after_us, pd.archive_after_us, pt.archive_after_us) IS NOT NULL",
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            if ages.is_empty() || limit == 0 {
                return Ok(Vec::new());
            }
            let tenants: Vec<Uuid> = ages.iter().map(|r| r.0).collect();
            let accounts: Vec<Uuid> = ages.iter().map(|r| r.1).collect();
            let age_us: Vec<i64> = ages.iter().map(|r| r.2).collect();
            let mut rows: Vec<DueRow> = Vec::new();
            for db in self.all_shards().await? {
                let mut tx = begin_system(&db.pool).await?;
                let part: Vec<DueRow> = sqlx::query_as(
                    "WITH RECURSIVE acc(tenant_id, account_id, age) AS (
                         SELECT t, a, us * interval '1 microsecond'
                         FROM unnest($1::uuid[], $2::uuid[], $3::bigint[]) AS u(t, a, us)
                     ), arch(tenant_id, account_id, mailbox_id) AS (
                         SELECT b.tenant_id, b.account_id, b.mailbox_id FROM mailboxes b
                         JOIN acc ON acc.tenant_id = b.tenant_id AND acc.account_id = b.account_id
                         WHERE b.role = 'archive'
                         UNION
                         SELECT c.tenant_id, c.account_id, c.mailbox_id FROM mailboxes c
                         JOIN arch p ON c.tenant_id = p.tenant_id AND c.account_id = p.account_id
                                    AND c.parent_id = p.mailbox_id
                     )
                     SELECT m.tenant_id, m.account_id, m.message_id, m.mailbox_id, m.received_at + acc.age
                     FROM messages m
                     JOIN acc ON acc.tenant_id = m.tenant_id AND acc.account_id = m.account_id
                     JOIN mailboxes mb ON mb.tenant_id = m.tenant_id AND mb.account_id = m.account_id
                                      AND mb.mailbox_id = m.mailbox_id
                     WHERE m.hidden_at IS NULL AND mb.role IS DISTINCT FROM 'trash'
                       AND m.received_at + acc.age <= $4
                       AND NOT EXISTS (SELECT 1 FROM arch WHERE arch.tenant_id = m.tenant_id
                                       AND arch.account_id = m.account_id AND arch.mailbox_id = m.mailbox_id)
                     ORDER BY 5, m.message_id
                     LIMIT $5",
                )
                .bind(&tenants)
                .bind(&accounts)
                .bind(&age_us)
                .bind(now)
                .bind(lim)
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?;
                crate::commits::commit(tx).await?;
                rows.extend(part);
            }
            rows.sort_by_key(|r| (r.4, r.2));
            rows.truncate(limit);
            Ok(rows
                .into_iter()
                .map(|(t, a, m, mb, due)| ArchiveItem {
                    scope: AccountScope {
                        tenant: TenantId::from_uuid(t),
                        account: AccountId::from_uuid(a),
                    },
                    message: MessageId::from_uuid(m),
                    mailbox: MailboxId::from_uuid(mb),
                    due,
                })
                .collect())
        })
        .await
    }

    async fn archive_usage(&self, scope: AccountScope) -> Result<QuotaUsage> {
        self.idempotent(|| async move {
            let db = self.box_db(scope).await?;
            self.usage_of(&db, scope, true).await
        })
        .await
    }
}
