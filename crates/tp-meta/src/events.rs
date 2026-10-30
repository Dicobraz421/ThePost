//! Журнал событий ядра: чтение разделов, позиции подписчиков, обработка под лизой, очистка
//! (CONTRACTS.md#Events, SRS 2.5).
//!
//! События пишут хранилища метаданных в транзакции изменения (`changelog::append_events`); здесь — чтение.
//! Позиции раздела становятся видимыми строго по возрастанию (счётчик раздела блокируется до фиксации),
//! поэтому чтение «после позиции» ничего не пропускает.
//!
//! [`EventConsumer`] обрабатывает раздел подписчика под лизой `events/{subscriber}/{partition}`:
//! позиция → пачка → [`EventSink::handle`] → продление лизы → фиксация позиции. Сбой до фиксации —
//! пачка придёт снова (не менее одного раза); порядок внутри раздела, а значит и ящика, сохраняется.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::events::{
    CoreEvent, CoreEventKind, EventLog, EventPos, EventSink, LoggedEvent, Partition,
};
use tp_contracts::lease::LeaseStore;
use tp_contracts::{AccountId, Error, MailboxId, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin_system, db_err, to_i64, to_u64};

/// Секция конфигурации подписчиков журнала событий `meta.events` (SRS 5.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EventsConfig {
    /// Событий в одной пачке подписчика.
    pub batch: usize,
    /// Срок лизы раздела подписчика: больше времени обработки пачки.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub lease_ttl: Duration,
    /// Событий, удаляемых одним вызовом очистки ([`PgMeta::purge_events`]).
    pub purge_batch: usize,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            batch: 256,
            lease_ttl: Duration::from_secs(30),
            purge_batch: 10_000,
        }
    }
}

#[derive(FromRow)]
struct EventRow {
    pos: i64,
    tenant_id: Uuid,
    account_id: Uuid,
    mailbox_id: Option<Uuid>,
    kind: String,
    at: OffsetDateTime,
}

impl EventRow {
    fn into_logged(self, partition: Partition) -> Result<LoggedEvent> {
        let kind: CoreEventKind = serde_json::from_str(&self.kind)
            .map_err(|e| Error::internal(format!("событие журнала: {e}")))?;
        Ok(LoggedEvent {
            partition,
            pos: EventPos(to_u64(self.pos)?),
            event: CoreEvent {
                tenant: TenantId::from_uuid(self.tenant_id),
                account: AccountId::from_uuid(self.account_id),
                mailbox: self.mailbox_id.map(MailboxId::from_uuid),
                kind,
                at: self.at,
            },
        })
    }
}

fn limit_arg(limit: usize) -> Result<i64> {
    i64::try_from(limit).map_err(|_| Error::invalid("слишком большой предел"))
}

impl PgMeta {
    fn partition_arg(&self, partition: Partition) -> Result<i32> {
        if partition.0 >= self.cfg.event_partitions {
            return Err(Error::invalid(format!("нет раздела {}", partition.0)));
        }
        i32::try_from(partition.0).map_err(|_| Error::invalid("номер раздела"))
    }

    /// Последняя выданная позиция раздела: подписчик, которому не нужна история, начинает с неё.
    pub async fn head(&self, partition: Partition) -> Result<EventPos> {
        let pos: Option<i64> =
            sqlx::query_scalar("SELECT last_pos FROM event_partitions WHERE partition = $1")
                .bind(self.partition_arg(partition)?)
                .fetch_optional(&self.pool)
                .await
                .map_err(db_err)?;
        Ok(EventPos(to_u64(pos.unwrap_or(0))?))
    }

    /// Удаляет не более `limit` событий, обработанных всеми `subscribers` (позиция не больше
    /// наименьшей зафиксированной; подписчик без позиции в разделе её не имеет — 0).
    /// Пустой список подписчиков ничего не удаляет. Возвращает число удалённых событий.
    pub async fn purge_events(&self, subscribers: &[&str], limit: usize) -> Result<u64> {
        let mut tx = begin_system(&self.pool).await?;
        let done = sqlx::query(
            "WITH subs AS (SELECT DISTINCT unnest($1::text[]) AS subscriber),
             floor AS (
                 SELECT p.partition, min(coalesce(c.pos, 0)) AS pos
                 FROM event_partitions p CROSS JOIN subs s
                 LEFT JOIN event_cursors c ON c.subscriber = s.subscriber AND c.partition = p.partition
                 GROUP BY p.partition),
             victims AS (
                 SELECT e.partition, e.pos FROM events e
                 JOIN floor f ON f.partition = e.partition AND e.pos <= f.pos
                 LIMIT $2)
             DELETE FROM events e USING victims v WHERE e.partition = v.partition AND e.pos = v.pos",
        )
        .bind(subscribers)
        .bind(limit_arg(limit)?)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(done.rows_affected())
    }
}

#[async_trait]
impl EventLog for PgMeta {
    fn partitions(&self) -> u32 {
        self.cfg.event_partitions
    }

    async fn read(
        &self,
        partition: Partition,
        after: EventPos,
        limit: usize,
    ) -> Result<Vec<LoggedEvent>> {
        let p = self.partition_arg(partition)?;
        let mut tx = begin_system(&self.pool).await?;
        let rows: Vec<EventRow> = sqlx::query_as(
            "SELECT pos, tenant_id, account_id, mailbox_id, kind::text AS kind, at FROM events
             WHERE partition = $1 AND pos > $2 ORDER BY pos LIMIT $3",
        )
        .bind(p)
        .bind(to_i64(after.0)?)
        .bind(limit_arg(limit)?)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        rows.into_iter().map(|r| r.into_logged(partition)).collect()
    }

    async fn position(&self, subscriber: &str, partition: Partition) -> Result<EventPos> {
        let pos: Option<i64> = sqlx::query_scalar(
            "SELECT pos FROM event_cursors WHERE subscriber = $1 AND partition = $2",
        )
        .bind(subscriber)
        .bind(self.partition_arg(partition)?)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(EventPos(to_u64(pos.unwrap_or(0))?))
    }

    async fn commit(&self, subscriber: &str, partition: Partition, pos: EventPos) -> Result<()> {
        sqlx::query(
            "INSERT INTO event_cursors (subscriber, partition, pos) VALUES ($1, $2, $3)
             ON CONFLICT (subscriber, partition) DO UPDATE
             SET pos = greatest(event_cursors.pos, EXCLUDED.pos), updated_at = now()",
        )
        .bind(subscriber)
        .bind(self.partition_arg(partition)?)
        .bind(to_i64(pos.0)?)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }
}

/// Обработчик разделов журнала для подписчиков одного экземпляра (узла и процесса).
///
/// Раздел подписчика обрабатывает один экземпляр — держатель лизы; лиза не освобождается между
/// вызовами, после сбоя экземпляра раздел переходит к другому по истечении [`EventsConfig::lease_ttl`].
#[derive(Clone)]
pub struct EventConsumer {
    log: Arc<dyn EventLog>,
    leases: Arc<dyn LeaseStore>,
    holder: String,
    cfg: EventsConfig,
}

impl std::fmt::Debug for EventConsumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventConsumer")
            .field("holder", &self.holder)
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl EventConsumer {
    /// Обработчик с держателем лиз `holder` (идентификатор узла и процесса).
    pub fn new(
        log: Arc<dyn EventLog>,
        leases: Arc<dyn LeaseStore>,
        holder: impl Into<String>,
        cfg: EventsConfig,
    ) -> Self {
        Self {
            log,
            leases,
            holder: holder.into(),
            cfg,
        }
    }

    /// Имя лизы раздела подписчика.
    pub fn lease_name(subscriber: &str, partition: Partition) -> String {
        format!("events/{subscriber}/{}", partition.0)
    }

    /// Обрабатывает одну пачку раздела. `None` — раздел обрабатывает другой экземпляр;
    /// иначе — число обработанных и зафиксированных событий. Ошибка подписчика — позиция не меняется.
    pub async fn poll_partition(
        &self,
        sink: &dyn EventSink,
        partition: Partition,
    ) -> Result<Option<usize>> {
        let name = Self::lease_name(sink.name(), partition);
        let ttl = self.cfg.lease_ttl;
        let Some(lease) = self.leases.try_acquire(&name, &self.holder, ttl).await? else {
            return Ok(None);
        };
        let after = self.log.position(sink.name(), partition).await?;
        let batch = self.log.read(partition, after, self.cfg.batch).await?;
        let Some(last) = batch.last().map(|e| e.pos) else {
            return Ok(Some(0));
        };
        sink.handle(&batch).await?;
        // Лиза потеряна во время обработки: позицию фиксирует новый держатель, пачка придёт ему снова.
        if self.leases.renew(&lease, ttl).await?.is_none() {
            tracing::warn!(
                subscriber = sink.name(),
                partition = partition.0,
                "лиза раздела потеряна"
            );
            return Ok(None);
        }
        self.log.commit(sink.name(), partition, last).await?;
        Ok(Some(batch.len()))
    }

    /// Одна пачка каждого раздела, доступного этому экземпляру; возвращает число обработанных событий.
    /// Ошибка подписчика в разделе прерывает проход (разделы после него — в следующем).
    pub async fn poll(&self, sink: &dyn EventSink) -> Result<usize> {
        let mut total = 0;
        for p in 0..self.log.partitions() {
            total += self.poll_partition(sink, Partition(p)).await?.unwrap_or(0);
        }
        Ok(total)
    }
}
