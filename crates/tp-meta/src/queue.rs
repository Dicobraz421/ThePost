//! Очередь исходящей почты на PostgreSQL (CONTRACTS.md#Queue (Ф1), SRS 4.1).
//!
//! Таблицы `queue` (письмо, лиза записи) и `queue_rcpts` (получатели и их состояние). Захват — одна
//! системная транзакция `UPDATE … FOR UPDATE SKIP LOCKED`: конкурентные узлы не получают одну запись дважды,
//! номер захвата (`fencing`) растёт при каждом захвате. Операции захвата (`renew`, `reschedule`,
//! `complete`) проверяют номер под блокировкой строки записи: устаревший — `Conflict`.
//!
//! Сроки попыток и лизы сравниваются с `now` вызывающего: часы СУБД не участвуют (кроме `queued_at`).
//! Очередь общая для организаций: `enqueue` — транзакция организации письма, остальное — системные транзакции.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;
use sqlx::{FromRow, PgConnection};
use time::OffsetDateTime;
use tp_contracts::queue::{
    Claimed, DsnNotify, DsnParams, DsnRet, OutMessage, OutRcpt, QueueClaim, QueueStore, QueuedRcpt,
    RcptStatus,
};
use tp_contracts::{BlobHash, BlobKey, Error, Gen, QueueId, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin, begin_system, db_err, to_i64, to_u32, to_u64};

fn ret_str(r: DsnRet) -> &'static str {
    match r {
        DsnRet::Full => "full",
        DsnRet::Headers => "hdrs",
    }
}

fn parse_ret(s: Option<String>) -> Result<Option<DsnRet>> {
    match s.as_deref() {
        None => Ok(None),
        Some("full") => Ok(Some(DsnRet::Full)),
        Some("hdrs") => Ok(Some(DsnRet::Headers)),
        Some(other) => Err(Error::internal(format!("queue.ret: {other}"))),
    }
}

/// `NOTIFY` → битовая маска столбца `notify` (1 — SUCCESS, 2 — FAILURE, 4 — DELAY).
fn notify_bits(n: Option<DsnNotify>) -> Option<i16> {
    n.map(|n| i16::from(n.success) | (i16::from(n.failure) << 1) | (i16::from(n.delay) << 2))
}

fn parse_notify(bits: Option<i16>) -> Option<DsnNotify> {
    bits.map(|b| DsnNotify {
        success: b & 1 != 0,
        failure: b & 2 != 0,
        delay: b & 4 != 0,
    })
}

/// Конец лизы `now + lease`.
fn lease_end(now: OffsetDateTime, lease: Duration) -> Result<OffsetDateTime> {
    time::Duration::try_from(lease)
        .ok()
        .and_then(|d| now.checked_add(d))
        .ok_or_else(|| Error::invalid("срок лизы записи очереди слишком велик"))
}

#[derive(FromRow)]
struct EntryRow {
    queue_id: Uuid,
    tenant_id: Uuid,
    blob_hash: Vec<u8>,
    blob_gen: i64,
    size: i64,
    mail_from: String,
    envid: Option<String>,
    ret: Option<String>,
    queued_at: OffsetDateTime,
    next_attempt: OffsetDateTime,
    fencing: i64,
    lease_until: Option<OffsetDateTime>,
}

#[derive(FromRow)]
struct RcptRow {
    queue_id: Uuid,
    address: String,
    notify: Option<i16>,
    orcpt: Option<String>,
    attempts: i32,
    last_error: Option<String>,
}

impl EntryRow {
    fn into_claimed(self, rcpts: Vec<QueuedRcpt>) -> Result<Claimed> {
        let tenant = TenantId::from_uuid(self.tenant_id);
        let hash: [u8; 32] = self
            .blob_hash
            .try_into()
            .map_err(|_| Error::internal("длина хэша в queue"))?;
        Ok(Claimed {
            claim: QueueClaim {
                id: QueueId::from_uuid(self.queue_id),
                fencing: to_u64(self.fencing)?,
            },
            until: self
                .lease_until
                .ok_or_else(|| Error::internal("захват без лизы"))?,
            queued_at: self.queued_at,
            tenant,
            blob: BlobKey {
                tenant,
                hash: BlobHash(hash),
                generation: Gen(to_u32(self.blob_gen)?),
            },
            size: to_u64(self.size)?,
            from: self.mail_from,
            dsn: DsnParams {
                envid: self.envid,
                ret: parse_ret(self.ret)?,
            },
            rcpts,
        })
    }
}

/// Блокирует запись захвата: номер устарел или записи нет — `Conflict`.
async fn lock_claim(tx: &mut PgConnection, claim: &QueueClaim) -> Result<()> {
    let found: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM queue WHERE queue_id = $1 AND fencing = $2 FOR UPDATE")
            .bind(claim.id.as_uuid())
            .bind(to_i64(claim.fencing)?)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
    found.map(|_| ()).ok_or(Error::Conflict)
}

/// Пересчитывает срок записи по незавершённым получателям; `false` — их не осталось, запись удалена.
async fn settle(tx: &mut PgConnection, id: QueueId) -> Result<bool> {
    let next: Option<OffsetDateTime> = sqlx::query_scalar(
        "SELECT min(next_attempt) FROM queue_rcpts WHERE queue_id = $1 AND status = 'pending'",
    )
    .bind(id.as_uuid())
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err)?;
    match next {
        Some(at) => {
            sqlx::query("UPDATE queue SET next_attempt = $2 WHERE queue_id = $1")
                .bind(id.as_uuid())
                .bind(at)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            Ok(true)
        }
        None => {
            sqlx::query("DELETE FROM queue WHERE queue_id = $1")
                .bind(id.as_uuid())
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            Ok(false)
        }
    }
}

#[async_trait]
impl QueueStore for PgMeta {
    async fn enqueue(&self, msg: OutMessage) -> Result<QueueId> {
        if msg.rcpts.is_empty() {
            return Err(Error::invalid("письмо в очередь без получателей"));
        }
        let mut seen = HashSet::with_capacity(msg.rcpts.len());
        if !msg.rcpts.iter().all(|r| seen.insert(r.address.as_str())) {
            return Err(Error::invalid("повтор адреса получателя"));
        }
        if msg.blob.tenant != msg.tenant {
            return Err(Error::invalid("объект письма другой организации"));
        }
        let id = QueueId::new();
        let mut tx = begin(&self.pool, msg.tenant).await?;
        sqlx::query(
            "INSERT INTO queue (tenant_id, queue_id, blob_hash, blob_gen, size, mail_from, envid, ret,
                                queued_at, next_attempt)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now(), $9)",
        )
        .bind(msg.tenant.as_uuid())
        .bind(id.as_uuid())
        .bind(msg.blob.hash.as_bytes().as_slice())
        .bind(i64::from(msg.blob.generation.0))
        .bind(to_i64(msg.size)?)
        .bind(&msg.from)
        .bind(&msg.dsn.envid)
        .bind(msg.dsn.ret.map(ret_str))
        .bind(msg.next_attempt)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        let addresses: Vec<&str> = msg.rcpts.iter().map(|r| r.address.as_str()).collect();
        let notify: Vec<Option<i16>> = msg.rcpts.iter().map(|r| notify_bits(r.notify)).collect();
        let orcpt: Vec<Option<&str>> = msg.rcpts.iter().map(|r| r.orcpt.as_deref()).collect();
        sqlx::query(
            "INSERT INTO queue_rcpts (tenant_id, queue_id, ord, address, notify, orcpt, next_attempt)
             SELECT $1, $2, r.ord, r.address, r.notify, r.orcpt, $6
             FROM unnest($3::text[], $4::int2[], $5::text[]) WITH ORDINALITY AS r(address, notify, orcpt, ord)",
        )
        .bind(msg.tenant.as_uuid())
        .bind(id.as_uuid())
        .bind(&addresses)
        .bind(&notify)
        .bind(&orcpt)
        .bind(msg.next_attempt)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(id)
    }

    async fn claim(
        &self,
        now: OffsetDateTime,
        limit: usize,
        lease: Duration,
    ) -> Result<Vec<Claimed>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let until = lease_end(now, lease)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut tx = begin_system(&self.pool).await?;
        // Строки, захваченные параллельным claim, пропускаются; после его фиксации условие перепроверяется
        // по новой версии строки (READ COMMITTED), поэтому запись с действующей лизой не выдаётся дважды.
        let mut entries: Vec<EntryRow> = sqlx::query_as(
            "WITH c AS (
                 SELECT queue_id FROM queue
                 WHERE next_attempt <= $1 AND (lease_until IS NULL OR lease_until <= $1)
                 ORDER BY next_attempt, queue_id
                 LIMIT $2
                 FOR UPDATE SKIP LOCKED
             )
             UPDATE queue q SET fencing = q.fencing + 1, lease_until = $3
             FROM c WHERE q.queue_id = c.queue_id
             RETURNING q.queue_id, q.tenant_id, q.blob_hash, q.blob_gen, q.size, q.mail_from, q.envid, q.ret,
                       q.queued_at, q.next_attempt, q.fencing, q.lease_until",
        )
        .bind(now)
        .bind(limit)
        .bind(until)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        entries.sort_by_key(|e| (e.next_attempt, e.queue_id));
        let ids: Vec<Uuid> = entries.iter().map(|e| e.queue_id).collect();
        let rows: Vec<RcptRow> = sqlx::query_as(
            "SELECT queue_id, address, notify, orcpt, attempts, last_error FROM queue_rcpts
             WHERE queue_id = ANY($1) AND status = 'pending' AND next_attempt <= $2
             ORDER BY queue_id, ord",
        )
        .bind(&ids)
        .bind(now)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        let mut rcpts: HashMap<Uuid, Vec<QueuedRcpt>> = HashMap::with_capacity(ids.len());
        for r in rows {
            rcpts.entry(r.queue_id).or_default().push(QueuedRcpt {
                rcpt: OutRcpt {
                    address: r.address,
                    notify: parse_notify(r.notify),
                    orcpt: r.orcpt,
                },
                attempts: u32::try_from(r.attempts)
                    .map_err(|_| Error::internal("отрицательное число попыток"))?,
                last_error: r.last_error,
            });
        }
        entries
            .into_iter()
            .map(|e| {
                let list = rcpts.remove(&e.queue_id).unwrap_or_default();
                e.into_claimed(list)
            })
            .collect()
    }

    async fn renew(&self, claim: &QueueClaim, until: OffsetDateTime) -> Result<()> {
        let mut tx = begin_system(&self.pool).await?;
        let n =
            sqlx::query("UPDATE queue SET lease_until = $3 WHERE queue_id = $1 AND fencing = $2")
                .bind(claim.id.as_uuid())
                .bind(to_i64(claim.fencing)?)
                .bind(until)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?
                .rows_affected();
        tx.commit().await.map_err(db_err)?;
        if n == 1 { Ok(()) } else { Err(Error::Conflict) }
    }

    async fn reschedule(
        &self,
        claim: &QueueClaim,
        rcpt: &str,
        at: OffsetDateTime,
        reason: &str,
    ) -> Result<()> {
        let mut tx = begin_system(&self.pool).await?;
        lock_claim(&mut tx, claim).await?;
        let n = sqlx::query(
            "UPDATE queue_rcpts SET next_attempt = $3, attempts = attempts + 1, last_error = $4
             WHERE queue_id = $1 AND address = $2 AND status = 'pending'",
        )
        .bind(claim.id.as_uuid())
        .bind(rcpt)
        .bind(at)
        .bind(reason)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n != 1 {
            return Err(Error::NotFound);
        }
        settle(&mut tx, claim.id).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn complete(&self, claim: &QueueClaim, rcpt: &str, status: RcptStatus) -> Result<bool> {
        let (state, reply) = match &status {
            RcptStatus::Delivered { reply } => ("delivered", reply),
            RcptStatus::Failed { reply } => ("failed", reply),
        };
        let mut tx = begin_system(&self.pool).await?;
        lock_claim(&mut tx, claim).await?;
        let n = sqlx::query(
            "UPDATE queue_rcpts SET status = $3, reply = $4
             WHERE queue_id = $1 AND address = $2 AND status = 'pending'",
        )
        .bind(claim.id.as_uuid())
        .bind(rcpt)
        .bind(state)
        .bind(reply)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n != 1 {
            return Err(Error::NotFound);
        }
        let pending = settle(&mut tx, claim.id).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(!pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_bits_roundtrip() {
        assert_eq!(notify_bits(None), None);
        assert_eq!(parse_notify(None), None);
        for b in 0..8i16 {
            let n = parse_notify(Some(b));
            assert_eq!(notify_bits(n), Some(b));
        }
        let never = DsnNotify::default();
        assert_eq!(notify_bits(Some(never)), Some(0));
    }

    #[test]
    fn ret_roundtrip() {
        for r in [DsnRet::Full, DsnRet::Headers] {
            assert_eq!(parse_ret(Some(ret_str(r).into())).expect("ret"), Some(r));
        }
        assert!(parse_ret(Some("x".into())).is_err());
    }

    #[test]
    fn lease_overflow() {
        let now = OffsetDateTime::UNIX_EPOCH;
        assert!(lease_end(now, Duration::from_secs(30)).is_ok());
        assert!(lease_end(now, Duration::MAX).is_err());
    }
}
