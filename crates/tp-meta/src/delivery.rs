//! Журнал доставки на PostgreSQL (META-22; CONTRACTS.md#DeliveryLog; SRS 5.2).
//!
//! Таблица `delivery_log` секционирована по суткам UTC; секции нужных суток создаются перед записью
//! (`tp_delivery_partitions`), устаревшие удаляются целиком ([`DeliveryLog::purge`], срок —
//! [`DeliveryLogConfig::retention`]). Таблица только на добавление: UPDATE и DELETE отклоняет триггер, роль
//! приложения имеет только SELECT и INSERT. Момент хранится с точностью до микросекунды.
//!
//! Позиция страницы — `<момент в микросекундах>.<номер записи>` последней записи страницы.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::{Date, OffsetDateTime, UtcOffset};
use tp_contracts::config::duration;
use tp_contracts::delivery::{
    DeliveryCursor, DeliveryDecision, DeliveryEvent, DeliveryEventKind, DeliveryLog, DeliveryPage,
    DeliveryQuery,
};
use tp_contracts::membership::NodeId;
use tp_contracts::{Error, QueueId, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::pool::{with_system, with_tenant};
use crate::schema::db_err;

/// Журнал доставки: секция `meta.delivery` (SRS 5.2, 5.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DeliveryLogConfig {
    /// Срок хранения записей; удаление — сутками ([`PgMeta::purge_expired_delivery`]).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retention: Duration,
}

impl Default for DeliveryLogConfig {
    fn default() -> Self {
        Self {
            retention: Duration::from_secs(30 * 24 * 3600),
        }
    }
}

fn kind_str(k: DeliveryEventKind) -> &'static str {
    match k {
        DeliveryEventKind::Receive => "receive",
        DeliveryEventKind::Reject => "reject",
        DeliveryEventKind::Deliver => "deliver",
        DeliveryEventKind::Relay => "relay",
        DeliveryEventKind::Dsn => "dsn",
    }
}

fn kind_of(s: &str) -> Result<DeliveryEventKind> {
    Ok(match s {
        "receive" => DeliveryEventKind::Receive,
        "reject" => DeliveryEventKind::Reject,
        "deliver" => DeliveryEventKind::Deliver,
        "relay" => DeliveryEventKind::Relay,
        "dsn" => DeliveryEventKind::Dsn,
        other => {
            return Err(Error::internal(format!(
                "событие журнала доставки: {other}"
            )));
        }
    })
}

fn decision_str(d: DeliveryDecision) -> &'static str {
    match d {
        DeliveryDecision::Accept => "accept",
        DeliveryDecision::Reject => "reject",
        DeliveryDecision::Defer => "defer",
        DeliveryDecision::Quarantine => "quarantine",
        DeliveryDecision::Discard => "discard",
        DeliveryDecision::Hold => "hold",
    }
}

fn decision_of(s: &str) -> Result<DeliveryDecision> {
    Ok(match s {
        "accept" => DeliveryDecision::Accept,
        "reject" => DeliveryDecision::Reject,
        "defer" => DeliveryDecision::Defer,
        "quarantine" => DeliveryDecision::Quarantine,
        "discard" => DeliveryDecision::Discard,
        "hold" => DeliveryDecision::Hold,
        other => {
            return Err(Error::internal(format!(
                "решение журнала доставки: {other}"
            )));
        }
    })
}

fn micros(at: OffsetDateTime) -> i128 {
    at.unix_timestamp_nanos().div_euclid(1000)
}

fn cursor(at: OffsetDateTime, seq: i64) -> DeliveryCursor {
    DeliveryCursor(format!("{}.{seq}", micros(at)))
}

fn parse_cursor(c: &DeliveryCursor) -> Result<(OffsetDateTime, i64)> {
    let bad = || Error::invalid("неверная позиция журнала доставки");
    let (at, seq) = c.0.split_once('.').ok_or_else(bad)?;
    let at: i128 = at.parse().map_err(|_| bad())?;
    let seq: i64 = seq.parse().map_err(|_| bad())?;
    let at = at
        .checked_mul(1000)
        .and_then(|n| OffsetDateTime::from_unix_timestamp_nanos(n).ok())
        .ok_or_else(bad)?;
    Ok((at, seq))
}

#[derive(FromRow)]
struct EventRow {
    at: OffsetDateTime,
    seq: i64,
    node: String,
    kind: String,
    sender: String,
    recipients: Vec<String>,
    message_id: Option<String>,
    queue_id: Option<Uuid>,
    decision: String,
    reason: Option<String>,
}

impl EventRow {
    fn into_event(self, tenant: TenantId) -> Result<DeliveryEvent> {
        Ok(DeliveryEvent {
            tenant,
            at: self.at,
            node: NodeId::new(self.node)?,
            kind: kind_of(&self.kind)?,
            sender: self.sender,
            recipients: self.recipients,
            message_id: self.message_id,
            queue_id: self.queue_id.map(QueueId::from_uuid),
            decision: decision_of(&self.decision)?,
            reason: self.reason,
        })
    }
}

/// Столбцы записи пачки (одна позиция массива — одна запись).
#[derive(Default)]
struct Batch {
    at: Vec<OffsetDateTime>,
    node: Vec<String>,
    kind: Vec<&'static str>,
    sender: Vec<String>,
    sender_key: Vec<String>,
    recipients: Vec<serde_json::Value>,
    rcpt_keys: Vec<serde_json::Value>,
    message_id: Vec<Option<String>>,
    queue_id: Vec<Option<Uuid>>,
    decision: Vec<&'static str>,
    reason: Vec<Option<String>>,
    days: Vec<Date>,
}

impl Batch {
    fn push(&mut self, e: DeliveryEvent) {
        let day = e.at.to_offset(UtcOffset::UTC).date();
        if !self.days.contains(&day) {
            self.days.push(day);
        }
        self.at.push(e.at);
        self.node.push(e.node.as_str().to_owned());
        self.kind.push(kind_str(e.kind));
        self.sender_key.push(e.sender.to_ascii_lowercase());
        self.sender.push(e.sender);
        self.rcpt_keys.push(serde_json::Value::from(
            e.recipients
                .iter()
                .map(|r| r.to_ascii_lowercase())
                .collect::<Vec<_>>(),
        ));
        self.recipients.push(serde_json::Value::from(e.recipients));
        self.message_id.push(e.message_id);
        self.queue_id.push(e.queue_id.map(|q| *q.as_uuid()));
        self.decision.push(decision_str(e.decision));
        self.reason.push(e.reason);
    }
}

impl PgMeta {
    /// Удаляет записи журнала доставки старше срока хранения ([`DeliveryLogConfig::retention`]) — роль
    /// maintenance.
    pub async fn purge_expired_delivery(&self) -> Result<()> {
        let retention = time::Duration::try_from(self.cfg.delivery.retention)
            .map_err(|_| Error::invalid("meta.delivery.retention вне диапазона"))?;
        let before = OffsetDateTime::now_utc()
            .checked_sub(retention)
            .ok_or_else(|| Error::invalid("meta.delivery.retention вне диапазона"))?;
        self.purge(before).await
    }

    async fn insert_delivery(&self, tenant: TenantId, batch: Batch) -> Result<()> {
        with_tenant(&self.pool, tenant, move |tx| {
            Box::pin(async move {
                sqlx::query("SELECT tp_delivery_partitions($1::date[])")
                    .bind(&batch.days)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                sqlx::query(
                    "INSERT INTO delivery_log (tenant_id, at, node, kind, sender, sender_key, recipients, rcpt_keys,
                                               message_id, queue_id, decision, reason)
                     SELECT $1, u.at, u.node, u.kind, u.sender, u.sender_key,
                            ARRAY(SELECT r FROM jsonb_array_elements_text(u.rcpt) WITH ORDINALITY AS x(r, i) ORDER BY i),
                            ARRAY(SELECT r FROM jsonb_array_elements_text(u.keys) WITH ORDINALITY AS x(r, i) ORDER BY i),
                            u.message_id, u.queue_id, u.decision, u.reason
                     FROM unnest($2::timestamptz[], $3::text[], $4::text[], $5::text[], $6::text[], $7::jsonb[],
                                 $8::jsonb[], $9::text[], $10::uuid[], $11::text[], $12::text[])
                          WITH ORDINALITY AS u(at, node, kind, sender, sender_key, rcpt, keys, message_id, queue_id,
                                               decision, reason, n)
                     ORDER BY u.n",
                )
                .bind(tenant.as_uuid())
                .bind(&batch.at)
                .bind(&batch.node)
                .bind(&batch.kind)
                .bind(&batch.sender)
                .bind(&batch.sender_key)
                .bind(&batch.recipients)
                .bind(&batch.rcpt_keys)
                .bind(&batch.message_id)
                .bind(&batch.queue_id)
                .bind(&batch.decision)
                .bind(&batch.reason)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                Ok(())
            })
        })
        .await
    }
}

#[async_trait]
impl DeliveryLog for PgMeta {
    async fn record(&self, event: DeliveryEvent) -> Result<()> {
        self.record_batch(vec![event]).await
    }

    /// Пачка — одна транзакция на организацию (порядок записи внутри организации сохраняется).
    async fn record_batch(&self, events: Vec<DeliveryEvent>) -> Result<()> {
        let mut batches: BTreeMap<TenantId, Batch> = BTreeMap::new();
        for e in events {
            batches.entry(e.tenant).or_default().push(e);
        }
        for (tenant, batch) in batches {
            self.insert_delivery(tenant, batch).await?;
        }
        Ok(())
    }

    async fn search(&self, tenant: TenantId, query: &DeliveryQuery) -> Result<DeliveryPage> {
        query.validate()?;
        let after = query.after.as_ref().map(parse_cursor).transpose()?;
        let limit = i64::try_from(query.limit).unwrap_or(i64::MAX - 1);
        let sender = query.sender.as_deref().map(str::to_ascii_lowercase);
        let recipient = query.recipient.as_deref().map(str::to_ascii_lowercase);
        let message_id = query.message_id.clone();
        let (since, until) = (query.since, query.until);
        let mut rows: Vec<EventRow> = with_tenant(&self.pool, tenant, move |tx| {
            Box::pin(async move {
                sqlx::query_as(
                    "SELECT at, seq, node, kind, sender, recipients, message_id, queue_id, decision, reason
                     FROM delivery_log
                     WHERE tenant_id = $1
                       AND ($2::text IS NULL OR sender_key = $2)
                       AND ($3::text IS NULL OR rcpt_keys @> ARRAY[$3::text])
                       AND ($4::text IS NULL OR message_id = $4)
                       AND ($5::timestamptz IS NULL OR at >= $5)
                       AND ($6::timestamptz IS NULL OR at < $6)
                       AND ($7::timestamptz IS NULL OR (at, seq) > ($7, $8::bigint))
                     ORDER BY at, seq LIMIT $9",
                )
                .bind(tenant.as_uuid())
                .bind(sender)
                .bind(recipient)
                .bind(message_id)
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
        .await?;
        let more = rows.len() > query.limit;
        rows.truncate(query.limit);
        let next = if more {
            rows.last().map(|r| cursor(r.at, r.seq))
        } else {
            None
        };
        Ok(DeliveryPage {
            events: rows
                .into_iter()
                .map(|r| r.into_event(tenant))
                .collect::<Result<_>>()?,
            next,
        })
    }

    async fn purge(&self, before: OffsetDateTime) -> Result<()> {
        let dropped: i32 = with_system(&self.pool, move |tx| {
            Box::pin(async move {
                sqlx::query_scalar("SELECT tp_delivery_purge($1)")
                    .bind(before)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(db_err)
            })
        })
        .await?;
        if dropped > 0 {
            tracing::info!(sections = dropped, %before, "журнал доставки: удалены секции");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_roundtrip() {
        let at = OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("time")
            + time::Duration::microseconds(123_456);
        let c = cursor(at, 42);
        assert_eq!(c.0, "1800000000123456.42");
        assert_eq!(parse_cursor(&c), Ok((at, 42)));
        for bad in ["", "1", "x.1", "1.x", "1.2.3"] {
            assert!(
                matches!(
                    parse_cursor(&DeliveryCursor(bad.into())),
                    Err(Error::Invalid(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn strings_roundtrip() {
        for k in [
            DeliveryEventKind::Receive,
            DeliveryEventKind::Reject,
            DeliveryEventKind::Deliver,
            DeliveryEventKind::Relay,
            DeliveryEventKind::Dsn,
        ] {
            assert_eq!(kind_of(kind_str(k)), Ok(k));
            assert_eq!(serde_json::to_value(k).expect("ser"), kind_str(k));
        }
        for d in [
            DeliveryDecision::Accept,
            DeliveryDecision::Reject,
            DeliveryDecision::Defer,
            DeliveryDecision::Quarantine,
            DeliveryDecision::Discard,
            DeliveryDecision::Hold,
        ] {
            assert_eq!(decision_of(decision_str(d)), Ok(d));
            assert_eq!(serde_json::to_value(d).expect("ser"), decision_str(d));
        }
    }

    #[test]
    fn default_retention_is_30_days() {
        assert_eq!(
            DeliveryLogConfig::default().retention,
            Duration::from_secs(30 * 24 * 3600)
        );
    }
}
