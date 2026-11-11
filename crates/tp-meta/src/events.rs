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
//! Ошибка раздела в [`EventConsumer::poll`] не прерывает проход: раздел повторяется не раньше своего
//! срока — пауза растёт вдвое от [`EventsConfig::retry_min`] до [`EventsConfig::retry_max`] (SRS 2.8),
//! ближайший срок — в [`PollReport::retry_in`].
//!
//! Шарды (META-31, CTR-34): журнал — на шарде ящика (события пишут транзакции ящика шарда), у каждого шарда
//! свои разделы `0..N` и позиции подписчиков. Журналы шардов — [`PgShardLogs`] ([`PgMeta::shard_logs`],
//! `ShardEventLogs`); раздел шарда обрабатывает [`EventConsumer`] этого шарда ([`EventConsumer::with_shard`]):
//! назначение и имя лизы — по ключу `shard_subscriber` (для шарда 0 — прежние). `EventLog` хранилищ со
//! справочником — журнал основной БД.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::events::{
    CoreEvent, CoreEventKind, EventLog, EventPos, EventSink, LoggedEvent, Partition,
    PartitionAssigner, ShardEventLogs, shard_subscriber,
};
use tp_contracts::lease::{Lease, LeaseStore};
use tp_contracts::membership::NodeId;
use tp_contracts::{AccountId, Error, MailboxId, Result, ShardId, TenantId};
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
    /// Первая пауза перед повтором раздела, опрос которого завершился ошибкой; каждая следующая
    /// ошибка подряд удваивает паузу (не больше [`EventsConfig::retry_max`]).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry_min: Duration,
    /// Наибольшая пауза перед повтором раздела с ошибкой; ограничивает и `retry_after` ошибки.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry_max: Duration,
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
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
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
        self.idempotent(|| async move {
            let pos: Option<i64> =
                sqlx::query_scalar("SELECT last_pos FROM event_partitions WHERE partition = $1")
                    .bind(self.partition_arg(partition)?)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(db_err)?;
            Ok(EventPos(to_u64(pos.unwrap_or(0))?))
        })
        .await
    }

    /// Удаляет не более `limit` событий, обработанных всеми `subscribers` (позиция не больше
    /// наименьшей зафиксированной; подписчик без позиции в разделе её не имеет — 0).
    /// Пустой список подписчиков ничего не удаляет. Возвращает число удалённых событий.
    ///
    /// Со справочником шардов — на каждом шарде (позиции подписчиков у каждого шарда свои), `limit` — на шард.
    pub async fn purge_events(&self, subscribers: &[&str], limit: usize) -> Result<u64> {
        if let Some(views) = self.shard_views().await? {
            let mut done = 0;
            for (_, view) in &views {
                done += Box::pin(view.purge_events(subscribers, limit)).await?;
            }
            return Ok(done);
        }
        self.rewrite(|| async move {
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
}).await
    }
}

/// Журналы событий шардов справочника (`ShardEventLogs`): снимок списка шардов на момент
/// [`PgMeta::shard_logs`]; после сигнала изменения справочника снимок строят заново.
#[derive(Clone)]
pub struct PgShardLogs {
    logs: BTreeMap<ShardId, Arc<dyn EventLog>>,
    partitions: u32,
}

impl std::fmt::Debug for PgShardLogs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgShardLogs")
            .field("shards", &self.logs.keys().collect::<Vec<_>>())
            .field("partitions", &self.partitions)
            .finish()
    }
}

impl ShardEventLogs for PgShardLogs {
    fn shards(&self) -> Vec<ShardId> {
        self.logs.keys().copied().collect()
    }

    fn log(&self, shard: ShardId) -> Option<Arc<dyn EventLog>> {
        self.logs.get(&shard).cloned()
    }

    fn partitions(&self) -> u32 {
        self.partitions
    }
}

impl PgMeta {
    /// Журналы событий всех шардов справочника (без справочника — один журнал основной БД).
    pub async fn shard_logs(&self) -> Result<PgShardLogs> {
        let ids = match self.shard_resolver() {
            Some(r) => r.shards().await?,
            None => vec![ShardId::MAIN],
        };
        let mut logs: BTreeMap<ShardId, Arc<dyn EventLog>> = BTreeMap::new();
        for id in ids {
            logs.insert(id, Arc::new(self.on_shard(id).await?));
        }
        Ok(PgShardLogs {
            logs,
            partitions: self.cfg.event_partitions,
        })
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
        self.idempotent(|| async move {
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
        })
        .await
    }

    async fn position(&self, subscriber: &str, partition: Partition) -> Result<EventPos> {
        self.idempotent(|| async move {
            let pos: Option<i64> = sqlx::query_scalar(
                "SELECT pos FROM event_cursors WHERE subscriber = $1 AND partition = $2",
            )
            .bind(subscriber)
            .bind(self.partition_arg(partition)?)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(EventPos(to_u64(pos.unwrap_or(0))?))
        })
        .await
    }

    /// Ключ записи (META-40) — сама позиция: после обрыва во время COMMIT она зафиксирована, если
    /// сохранённая не меньше (позиция только растёт).
    async fn commit(&self, subscriber: &str, partition: Partition, pos: EventPos) -> Result<()> {
        let p = self.partition_arg(partition)?;
        let pos = to_i64(pos.0)?;
        self.keyed_system(
            |tx| {
                let subscriber = subscriber.to_owned();
                Box::pin(async move {
                    sqlx::query(
                        "INSERT INTO event_cursors (subscriber, partition, pos) VALUES ($1, $2, $3)
                         ON CONFLICT (subscriber, partition) DO UPDATE
                         SET pos = greatest(event_cursors.pos, EXCLUDED.pos), updated_at = now()",
                    )
                    .bind(subscriber)
                    .bind(p)
                    .bind(pos)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                    Ok(())
                })
            },
            |tx| {
                let subscriber = subscriber.to_owned();
                Box::pin(async move {
                    let done: bool = sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM event_cursors
                                        WHERE subscriber = $1 AND partition = $2 AND pos >= $3)",
                    )
                    .bind(subscriber)
                    .bind(p)
                    .bind(pos)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(db_err)?;
                    Ok(done.then_some(()))
                })
            },
        )
        .await
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

/// Повтор раздела с ошибкой: ошибок подряд и срок, раньше которого раздел не опрашивается.
#[derive(Debug, Clone, Copy)]
struct Retry {
    failures: u32,
    due: Instant,
}

/// Разделы с ошибкой: (подписчик, раздел) → повтор.
type Retries = HashMap<(String, u32), Retry>;

/// Итог прохода [`EventConsumer::poll`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PollReport {
    /// Обработано и зафиксировано событий.
    pub handled: usize,
    /// Разделы, опрос которых в этом проходе завершился ошибкой (позиция не сдвинута).
    pub failed: Vec<(Partition, Error)>,
    /// Разделов с ошибкой, пропущенных до срока повтора.
    pub deferred: usize,
    /// Через сколько наступит ближайший срок повтора раздела с ошибкой; `None` — таких разделов нет.
    pub retry_in: Option<Duration>,
}

/// Пауза перед повтором после `failures` ошибок подряд: `retry_min · 2^(failures−1)`, не меньше
/// `retry_after` ошибки и не больше `retry_max`.
fn retry_pause(cfg: &EventsConfig, failures: u32, err: &Error) -> Duration {
    let factor = 1u32
        .checked_shl(failures.saturating_sub(1))
        .unwrap_or(u32::MAX);
    let mut pause = cfg.retry_min.saturating_mul(factor);
    if let Error::Unavailable {
        retry_after: Some(after),
    } = err
    {
        pause = pause.max(*after);
    }
    pause.min(cfg.retry_max.max(cfg.retry_min))
}

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
    shard: ShardId,
    assignment: Option<Assignment>,
    held: Arc<Mutex<Held>>,
    retries: Arc<Mutex<Retries>>,
}

impl std::fmt::Debug for EventConsumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventConsumer")
            .field("holder", &self.holder)
            .field("shard", &self.shard)
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
            shard: ShardId::MAIN,
            assignment: None,
            held: Arc::new(Mutex::new(HashMap::new())),
            retries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Обрабатывать только разделы, назначенные узлу `node` распределителем `assigner`.
    pub fn with_assigner(mut self, assigner: Arc<dyn PartitionAssigner>, node: NodeId) -> Self {
        self.assignment = Some(Assignment { assigner, node });
        self
    }

    /// Журнал `log` — журнал шарда `shard` (`ShardEventLogs::log`): назначение разделов и имена лиз — по ключу
    /// подписчика шарда `shard_subscriber(subscriber, shard)`; шард 0 — прежние.
    pub fn with_shard(mut self, shard: ShardId) -> Self {
        self.shard = shard;
        self
    }

    /// Шард журнала.
    pub fn shard(&self) -> ShardId {
        self.shard
    }

    /// Имя лизы раздела подписчика (журнал шарда 0).
    pub fn lease_name(subscriber: &str, partition: Partition) -> String {
        format!("events/{subscriber}/{}", partition.0)
    }

    /// Имя лизы раздела подписчика журнала этого обработчика: `events/{ключ подписчика шарда}/{раздел}`.
    pub fn shard_lease_name(&self, subscriber: &str, partition: Partition) -> String {
        Self::lease_name(&shard_subscriber(subscriber, self.shard), partition)
    }

    /// Разделы подписчика, назначенные этому экземпляру при текущем составе, по возрастанию.
    pub fn assigned(&self, subscriber: &str) -> Vec<Partition> {
        let partitions = self.log.partitions();
        match &self.assignment {
            None => (0..partitions).map(Partition).collect(),
            Some(a) => a.assigner.assigned(
                &shard_subscriber(subscriber, self.shard),
                &a.node,
                partitions,
            ),
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
        let name = self.shard_lease_name(sink.name(), partition);
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

    /// Одна пачка каждого раздела, назначенного этому экземпляру.
    /// Сначала отдаёт лизы разделов, снятых с экземпляра (их пачки уже зафиксированы предыдущим проходом).
    ///
    /// Ошибка раздела не прерывает проход: она попадает в [`PollReport::failed`] (и в `warn` — один раз
    /// за попытку, то есть не чаще раза за паузу), позиция раздела не меняется, а сам раздел до срока
    /// повтора пропускается (SRS 2.8; паузы — [`EventsConfig::retry_min`], [`EventsConfig::retry_max`]).
    /// Успешный опрос раздела сбрасывает паузу. Ошибка всего прохода сейчас не возникает.
    pub async fn poll(&self, sink: &dyn EventSink) -> Result<PollReport> {
        let subscriber = sink.name();
        let assigned = self.assigned(subscriber);
        let mut report = PollReport::default();
        for p in self.held(subscriber) {
            if assigned.binary_search(&p).is_err()
                && let Err(e) = self.release(subscriber, p).await
            {
                tracing::warn!(subscriber, partition = p.0, error = %e, "лиза раздела не отдана");
                report.failed.push((p, e));
            }
        }
        // Разделы, снятые с экземпляра, больше не повторяются здесь.
        self.retries_map()
            .retain(|(s, p), _| s != subscriber || assigned.binary_search(&Partition(*p)).is_ok());
        for p in assigned {
            let key = (subscriber.to_owned(), p.0);
            let now = Instant::now();
            let retry = self.retries_map().get(&key).copied();
            if let Some(r) = retry
                && r.due > now
            {
                report.deferred += 1;
                continue;
            }
            match self.poll_assigned(sink, p).await {
                Ok(n) => {
                    self.retries_map().remove(&key);
                    report.handled += n.unwrap_or(0);
                }
                Err(e) => {
                    let failures = retry.map_or(0, |r| r.failures).saturating_add(1);
                    let pause = retry_pause(&self.cfg, failures, &e);
                    tracing::warn!(
                        subscriber,
                        partition = p.0,
                        failures,
                        retry_in = ?pause,
                        error = %e,
                        "раздел журнала не обработан, повтор по сроку"
                    );
                    self.retries_map().insert(
                        key,
                        Retry {
                            failures,
                            due: Instant::now() + pause,
                        },
                    );
                    report.failed.push((p, e));
                }
            }
        }
        report.retry_in = self.retry_in(subscriber);
        Ok(report)
    }

    /// Через сколько наступит ближайший срок повтора раздела подписчика с ошибкой (`ZERO` — уже).
    pub fn retry_in(&self, subscriber: &str) -> Option<Duration> {
        let now = Instant::now();
        self.retries_map()
            .iter()
            .filter(|((s, _), _)| s == subscriber)
            .map(|(_, r)| r.due.saturating_duration_since(now))
            .min()
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

    fn retries_map(&self) -> MutexGuard<'_, Retries> {
        // Как и `held`: каждая правка карты — одна операция.
        self.retries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn held_map(&self) -> MutexGuard<'_, Held> {
        // Карта согласована при любой панике держателя: каждая правка — одна операция.
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use tp_contracts::events::partition_of;
    use tp_testkit::infra::{MemEventLog, MemLease};

    use super::*;

    /// Подписчик, отказывающий в пачках раздела 0, пока `broken`.
    struct Sink {
        broken: AtomicBool,
        seen: Mutex<Vec<LoggedEvent>>,
    }

    #[async_trait]
    impl EventSink for Sink {
        fn name(&self) -> &str {
            "test"
        }

        async fn handle(&self, batch: &[LoggedEvent]) -> Result<()> {
            if self.broken.load(Ordering::SeqCst) && batch.iter().any(|e| e.partition.0 == 0) {
                return Err(Error::Unavailable {
                    retry_after: Some(Duration::from_millis(1)),
                });
            }
            self.seen.lock().expect("lock").extend_from_slice(batch);
            Ok(())
        }
    }

    fn event(account: AccountId) -> CoreEvent {
        CoreEvent {
            tenant: TenantId::new(),
            account,
            mailbox: None,
            kind: CoreEventKind::MailboxCreated,
            at: OffsetDateTime::now_utc(),
        }
    }

    /// Журнал с двумя разделами и событием в каждом.
    fn log() -> Arc<MemEventLog> {
        let log = Arc::new(MemEventLog::new(2));
        let mut filled = [false; 2];
        while filled != [true, true] {
            let account = AccountId::new();
            let p = partition_of(account, 2).0 as usize;
            if !filled[p] {
                log.append(event(account));
                filled[p] = true;
            }
        }
        log
    }

    fn consumer(log: Arc<MemEventLog>, retry_min: Duration) -> EventConsumer {
        EventConsumer::new(
            log,
            Arc::new(MemLease::default()),
            "a",
            EventsConfig {
                retry_min,
                retry_max: retry_min * 4,
                ..EventsConfig::default()
            },
        )
    }

    #[tokio::test]
    async fn failed_partition_does_not_stop_pass() {
        let sink = Sink {
            broken: AtomicBool::new(true),
            seen: Mutex::new(Vec::new()),
        };
        let c = consumer(log(), Duration::ZERO);
        let r = c.poll(&sink).await.expect("poll");
        assert_eq!(r.handled, 1);
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.failed[0].0, Partition(0));
        assert_eq!(r.retry_in, Some(Duration::ZERO));

        // Срок наступил — раздел повторяется; успех сбрасывает повтор.
        sink.broken.store(false, Ordering::SeqCst);
        let r = c.poll(&sink).await.expect("poll");
        assert_eq!((r.handled, r.failed.len(), r.retry_in), (1, 0, None));
        assert_eq!(sink.seen.lock().expect("lock").len(), 2);
    }

    #[tokio::test]
    async fn failed_partition_waits_for_due() {
        let sink = Sink {
            broken: AtomicBool::new(true),
            seen: Mutex::new(Vec::new()),
        };
        let c = consumer(log(), Duration::from_secs(3600));
        assert_eq!(c.poll(&sink).await.expect("poll").failed.len(), 1);
        sink.broken.store(false, Ordering::SeqCst);
        let r = c.poll(&sink).await.expect("poll");
        assert_eq!((r.handled, r.deferred, r.failed.len()), (0, 1, 0));
        assert!(r.retry_in.expect("срок") > Duration::from_secs(3000));
    }

    #[test]
    fn pause_doubles_up_to_max() {
        let cfg = EventsConfig {
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(5),
            ..EventsConfig::default()
        };
        let e = Error::UNAVAILABLE;
        let pauses: Vec<_> = (1..=5)
            .map(|n| retry_pause(&cfg, n, &e).as_secs())
            .collect();
        assert_eq!(pauses, [1, 2, 4, 5, 5]);
        assert_eq!(retry_pause(&cfg, 100, &e), Duration::from_secs(5));
        let hinted = Error::Unavailable {
            retry_after: Some(Duration::from_secs(3)),
        };
        assert_eq!(retry_pause(&cfg, 1, &hinted), Duration::from_secs(3));
        let huge = Error::Unavailable {
            retry_after: Some(Duration::from_secs(600)),
        };
        assert_eq!(retry_pause(&cfg, 1, &huge), Duration::from_secs(5));
    }
}
