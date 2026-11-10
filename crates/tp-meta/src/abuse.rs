//! Блокировки источников и учётных записей на PostgreSQL (META-27; CONTRACTS.md#AbuseBlocks, SRS 6.3,
//! решение 2026-11-01 «Взаимодействие компонентов»).
//!
//! Таблица `abuse_blocks` организации (RLS): субъект — ключ строки, адрес и подсеть — также `cidr` для поиска
//! покрывающих подсетей (`>>=`). Срок — часы СУБД. После фиксации [`AbuseBlocks::block`] публикуется сигнал
//! `Blocked { subject }`, после фиксации [`AbuseBlocks::unblock`] снятия действующей блокировки — `Unblocked { subject }`
//! (META-39; [`PgMeta::with_bus`]); узлы держат локальный набор ([`AbuseBlocks::active`]).
//! Истёкшие строки не действуют; [`AbuseBlocks::block`] удаляет до [`AbuseBlocksConfig::purge_batch`] истёкших
//! строк организации в той же транзакции — таблица не растёт от разовых источников.

use std::net::IpAddr;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::OffsetDateTime;
use tp_contracts::abuse::{AbuseBlock, AbuseBlocks, BlockSubject};
use tp_contracts::bus::{Event, EventKind};
use tp_contracts::{AccountId, Error, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin, db_err};

/// Подсекция `meta.abuse` (SRS 5.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AbuseBlocksConfig {
    /// Истёкших блокировок организации, удаляемых при каждой новой блокировке.
    pub purge_batch: u32,
}

impl Default for AbuseBlocksConfig {
    fn default() -> Self {
        Self { purge_batch: 100 }
    }
}

/// Наибольшая длина причины (как в схеме).
const REASON_MAX: usize = 1024;

/// Субъект в столбцах таблицы: ключ, вид, подсеть (`cidr`), учётная запись.
struct Columns {
    key: String,
    kind: &'static str,
    net: Option<String>,
    account: Option<Uuid>,
}

fn columns(subject: &BlockSubject) -> Columns {
    match *subject {
        BlockSubject::Ip(ip) => {
            let bits = if ip.is_ipv4() { 32 } else { 128 };
            Columns {
                key: format!("ip:{ip}"),
                kind: "ip",
                net: Some(format!("{ip}/{bits}")),
                account: None,
            }
        }
        BlockSubject::Net { addr, prefix } => Columns {
            key: format!("net:{addr}/{prefix}"),
            kind: "net",
            net: Some(format!("{addr}/{prefix}")),
            account: None,
        },
        BlockSubject::Account(a) => Columns {
            key: format!("account:{}", a.as_uuid()),
            kind: "account",
            net: None,
            account: Some(*a.as_uuid()),
        },
    }
}

#[derive(FromRow)]
struct Row {
    kind: String,
    host: Option<String>,
    prefix: Option<i32>,
    account_id: Option<Uuid>,
    until: OffsetDateTime,
    reason: String,
}

impl Row {
    fn subject(&self) -> Result<BlockSubject> {
        let bad = || Error::internal(format!("блокировка: неверный субъект вида {}", self.kind));
        let addr = || -> Result<IpAddr> {
            self.host
                .as_deref()
                .ok_or_else(bad)?
                .parse()
                .map_err(|_| bad())
        };
        Ok(match self.kind.as_str() {
            "ip" => BlockSubject::Ip(addr()?),
            "net" => {
                let prefix = self
                    .prefix
                    .and_then(|p| u8::try_from(p).ok())
                    .ok_or_else(bad)?;
                BlockSubject::net(addr()?, prefix).map_err(|_| bad())?
            }
            "account" => {
                BlockSubject::Account(AccountId::from_uuid(self.account_id.ok_or_else(bad)?))
            }
            _ => return Err(bad()),
        })
    }
}

#[async_trait]
impl AbuseBlocks for PgMeta {
    async fn block(
        &self,
        tenant: TenantId,
        subject: &BlockSubject,
        until: OffsetDateTime,
        reason: &str,
    ) -> Result<()> {
        self.rewrite(|| async move {
        if reason.len() > REASON_MAX {
            return Err(Error::invalid("причина блокировки длиннее 1024 байт"));
        }
        let c = columns(subject);
        let mut tx = begin(&self.pool, tenant).await?;
        sqlx::query(
            "DELETE FROM abuse_blocks WHERE ctid IN (
                 SELECT ctid FROM abuse_blocks WHERE tenant_id = $1 AND until <= now() AND subject <> $2 LIMIT $3)",
        )
        .bind(tenant.as_uuid())
        .bind(&c.key)
        .bind(i64::from(self.cfg.abuse.purge_batch))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        // Действующая блокировка не сокращается; истёкшая заменяется новой. Срок в прошлом — строки нет.
        let done: Option<i32> = sqlx::query_scalar(
            "INSERT INTO abuse_blocks (tenant_id, subject, kind, net, account_id, until, reason)
             SELECT $1::uuid, $2::text, $3::text, $4::cidr, $5::uuid, $6::timestamptz, $7::text
             WHERE $6::timestamptz > now()
             ON CONFLICT (tenant_id, subject) DO UPDATE SET
                 until = CASE WHEN abuse_blocks.until > now()
                              THEN greatest(abuse_blocks.until, EXCLUDED.until) ELSE EXCLUDED.until END,
                 reason = EXCLUDED.reason,
                 blocked_at = now()
             RETURNING 1",
        )
        .bind(tenant.as_uuid())
        .bind(&c.key)
        .bind(c.kind)
        .bind(c.net)
        .bind(c.account)
        .bind(until)
        .bind(reason)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        if done.is_none() {
            return Err(Error::invalid("срок блокировки в прошлом"));
        }
        tx.commit().await.map_err(db_err)?;
        self.signal(Event {
            tenant,
            account: None,
            kind: EventKind::Blocked { subject: *subject },
        })
        .await;
        Ok(())
}).await
    }

    async fn blocked(
        &self,
        tenant: TenantId,
        subject: &BlockSubject,
    ) -> Result<Option<OffsetDateTime>> {
        self.idempotent(|| async move {
            let c = columns(subject);
            let mut tx = begin(&self.pool, tenant).await?;
            // Тот же субъект или подсеть, покрывающая адрес или подсеть (разные семейства адресов не совпадают).
            let until: Option<OffsetDateTime> = sqlx::query_scalar(
                "SELECT max(until) FROM abuse_blocks
             WHERE tenant_id = $1 AND until > now()
               AND (subject = $2 OR (kind = 'net' AND $3::cidr IS NOT NULL AND net >>= $3::cidr))",
            )
            .bind(tenant.as_uuid())
            .bind(&c.key)
            .bind(c.net)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            Ok(until)
        })
        .await
    }

    async fn unblock(&self, tenant: TenantId, subject: &BlockSubject) -> Result<()> {
        // Удаление по ключу идемпотентно ([`PgMeta::rewrite`]): повтор после обрыва во время COMMIT удалит ноль строк.
        self.rewrite(|| async move {
            let c = columns(subject);
            let mut tx = begin(&self.pool, tenant).await?;
            sqlx::query("DELETE FROM abuse_blocks WHERE tenant_id = $1 AND subject = $2")
                .bind(tenant.as_uuid())
                .bind(&c.key)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            Ok(())
        })
        .await?;
        // Сигнал — один раз после фиксации, и когда строки не было (контракт это допускает): повтор после обрыва во
        // время COMMIT удалённой строки уже не видит, а сигнал терять нельзя; лишний `Unblocked` узлу безвреден.
        self.signal(Event {
            tenant,
            account: None,
            kind: EventKind::Unblocked { subject: *subject },
        })
        .await;
        Ok(())
    }

    async fn active(&self, tenant: TenantId) -> Result<Vec<AbuseBlock>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            let rows: Vec<Row> = sqlx::query_as(
                "SELECT kind, host(net) AS host, masklen(net) AS prefix, account_id, until, reason
             FROM abuse_blocks WHERE tenant_id = $1 AND until > now()",
            )
            .bind(tenant.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            rows.into_iter()
                .map(|r| {
                    Ok(AbuseBlock {
                        subject: r.subject()?,
                        until: r.until,
                        reason: r.reason,
                    })
                })
                .collect()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_columns() {
        let ip: IpAddr = "192.0.2.7".parse().expect("ip");
        let c = columns(&BlockSubject::Ip(ip));
        assert_eq!((c.key.as_str(), c.kind), ("ip:192.0.2.7", "ip"));
        assert_eq!(c.net.as_deref(), Some("192.0.2.7/32"));
        let c = columns(&BlockSubject::net("2001:db8::1".parse().expect("ip"), 48).expect("net"));
        assert_eq!(c.key, "net:2001:db8::/48");
        assert_eq!(c.net.as_deref(), Some("2001:db8::/48"));
        let a = AccountId::new();
        let c = columns(&BlockSubject::Account(a));
        assert_eq!(c.account, Some(*a.as_uuid()));
        assert!(c.net.is_none());
        // Адрес /32 и подсеть /32 — разные субъекты.
        let net32 = BlockSubject::net(ip, 32).expect("net");
        assert_ne!(columns(&net32).key, columns(&BlockSubject::Ip(ip)).key);
    }
}
