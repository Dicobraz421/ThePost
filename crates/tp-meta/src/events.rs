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
//! Между экземплярами разделы распределяет [`PartitionAssigner`] по живым узлам ([`crate::membership`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::events::{
    CoreEvent, CoreEventKind, EventLog, EventPos, EventSink, LoggedEvent, Partition,
    PartitionAssigner,
};
use tp_contracts::lease::{Lease, LeaseStore};
use tp_contracts::membership::NodeId;
use tp_contracts::{AccountId, Error, MailboxId, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin_system, db_err, to_i64, to_u64};

/// Секция конфигурации подписчиков журнала событий и членства узлов `meta.events` (SRS 5.5).
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
    /// Срок записи узла в членстве (`Membership::heartbeat`); узел продлевает её с периодом `node_ttl / 3`.
    /// Меньше — быстрее переход разделов упавшего узла, больше — устойчивее к паузам СУБД и сети.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub node_ttl: Duration,
}

impl EventsConfig {
    /// Период продления записи узла: треть [`EventsConfig::node_ttl`].
    pub fn heartbeat_interval(&self) -> Duration {
        self.node_ttl / 3
    }
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            batch: 256,
            lease_ttl: Duration::from_secs(30),
            purge_batch: 10_000,
            node_ttl: Duration::from_secs(15),
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

/// Назначение разделов узлу: распределитель и имя этого узла.
#[derive(Clone)]
struct Assignment {
    assigner: Arc<dyn PartitionAssigner>,
    node: NodeId,
}

/// Лизы разделов, которые держит экземпляр: (подписчик, раздел) → последняя полученная лиза.
type Held = HashMap<(String, u32), Lease>;

/// Обработчик разделов журнала для подписчиков одного экземпляра (узла и процесса).
///
/// Раздел подписчика обрабатывает один экземпляр — держатель лизы; лиза не освобождается между
/// вызовами, после сбоя экземпляра раздел переходит к другому по истечении [`EventsConfig::lease_ttl`].
///
/// С распределителем ([`EventConsumer::with_assigner`], решение 2026-10-30, #45) экземпляр берёт лизы
/// только назначенных его узлу разделов, а лизы разделов, снятых с узла, отдаёт после фиксации текущей
/// пачки (в следующем [`EventConsumer::poll`]); узел вне состава отдаёт все. Без распределителя
/// экземпляру доступны все разделы (один узел).
#[derive(Clone)]
pub struct EventConsumer {
    log: Arc<dyn EventLog>,
    leases: Arc<dyn LeaseStore>,
    holder: String,
    cfg: EventsConfig,
    assignment: Option<Assignment>,
    held: Arc<Mutex<Held>>,
}

impl std::fmt::Debug for EventConsumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventConsumer")
            .field("holder", &self.holder)
            .field("node", &self.assignment.as_ref().map(|a| &a.node))
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl EventConsumer {
    /// Обработчик с держателем лиз `holder` (идентификатор узла и процесса); доступны все разделы.
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
            assignment: None,
            held: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Обрабатывать только разделы, назначенные узлу `node` распределителем `assigner`.
    pub fn with_assigner(mut self, assigner: Arc<dyn PartitionAssigner>, node: NodeId) -> Self {
        self.assignment = Some(Assignment { assigner, node });
        self
    }

    /// Имя лизы раздела подписчика.
    pub fn lease_name(subscriber: &str, partition: Partition) -> String {
        format!("events/{subscriber}/{}", partition.0)
    }

    /// Разделы подписчика, назначенные этому экземпляру при текущем составе, по возрастанию.
    pub fn assigned(&self, subscriber: &str) -> Vec<Partition> {
        let partitions = self.log.partitions();
        match &self.assignment {
            None => (0..partitions).map(Partition).collect(),
            Some(a) => a.assigner.assigned(subscriber, &a.node, partitions),
        }
    }

    /// Разделы подписчика, лизы которых держит экземпляр, по возрастанию.
    pub fn held(&self, subscriber: &str) -> Vec<Partition> {
        let mut out: Vec<_> = self
            .held_map()
            .keys()
            .filter(|(s, _)| s == subscriber)
            .map(|(_, p)| Partition(*p))
            .collect();
        out.sort();
        out
    }

    /// Обрабатывает одну пачку раздела. `None` — раздел обрабатывает другой экземпляр или он не назначен
    /// этому (лиза неназначенного раздела отдаётся); иначе — число обработанных и зафиксированных событий.
    /// Ошибка подписчика — позиция не меняется.
    pub async fn poll_partition(
        &self,
        sink: &dyn EventSink,
        partition: Partition,
    ) -> Result<Option<usize>> {
        if self.assignment.is_some() && !self.assigned(sink.name()).contains(&partition) {
            self.release(sink.name(), partition).await?;
            return Ok(None);
        }
        self.poll_assigned(sink, partition).await
    }

    async fn poll_assigned(
        &self,
        sink: &dyn EventSink,
        partition: Partition,
    ) -> Result<Option<usize>> {
        let key = (sink.name().to_owned(), partition.0);
        let name = Self::lease_name(sink.name(), partition);
        let ttl = self.cfg.lease_ttl;
        let Some(lease) = self.leases.try_acquire(&name, &self.holder, ttl).await? else {
            self.held_map().remove(&key);
            return Ok(None);
        };
        self.held_map().insert(key.clone(), lease.clone());
        let after = self.log.position(sink.name(), partition).await?;
        let batch = self.log.read(partition, after, self.cfg.batch).await?;
        let Some(last) = batch.last().map(|e| e.pos) else {
            return Ok(Some(0));
        };
        sink.handle(&batch).await?;
        // Лиза потеряна во время обработки: позицию фиксирует новый держатель, пачка придёт ему снова.
        let Some(lease) = self.leases.renew(&lease, ttl).await? else {
            self.held_map().remove(&key);
            tracing::warn!(
                subscriber = sink.name(),
                partition = partition.0,
                "лиза раздела потеряна"
            );
            return Ok(None);
        };
        self.held_map().insert(key, lease);
        self.log.commit(sink.name(), partition, last).await?;
        Ok(Some(batch.len()))
    }

    /// Одна пачка каждого раздела, назначенного этому экземпляру; возвращает число обработанных событий.
    /// Сначала отдаёт лизы разделов, снятых с экземпляра (их пачки уже зафиксированы предыдущим проходом).
    /// Ошибка подписчика в разделе прерывает проход (разделы после него — в следующем).
    pub async fn poll(&self, sink: &dyn EventSink) -> Result<usize> {
        let assigned = self.assigned(sink.name());
        for p in self.held(sink.name()) {
            if assigned.binary_search(&p).is_err() {
                self.release(sink.name(), p).await?;
            }
        }
        let mut total = 0;
        for p in assigned {
            total += self.poll_assigned(sink, p).await?.unwrap_or(0);
        }
        Ok(total)
    }

    /// Отдаёт все лизы подписчика (штатная остановка): разделы переходят к другим узлам без ожидания срока.
    pub async fn release_all(&self, subscriber: &str) -> Result<()> {
        for p in self.held(subscriber) {
            self.release(subscriber, p).await?;
        }
        Ok(())
    }

    async fn release(&self, subscriber: &str, partition: Partition) -> Result<()> {
        let lease = self
            .held_map()
            .remove(&(subscriber.to_owned(), partition.0));
        if let Some(lease) = lease {
            tracing::debug!(
                subscriber,
                partition = partition.0,
                "лиза раздела отдана: раздел снят с узла"
            );
            self.leases.release(&lease).await?;
        }
        Ok(())
    }

    fn held_map(&self) -> MutexGuard<'_, Held> {
        // Карта согласована при любой панике держателя: каждая правка — одна операция.
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
