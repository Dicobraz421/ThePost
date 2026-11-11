//! In-memory шина уведомлений, лизы, журнал событий ядра (в том числе по шардам), назначение разделов
//! и часы (CONTRACTS.md#EventBus, #Lease, #Events).
//!
//! Время реализаций testkit берётся из [`Clock`]: в тестах таймаутов — [`ManualClock`], который
//! двигается только явно ([`ManualClock::advance`]), иначе — [`SystemClock`].

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use time::OffsetDateTime;
use tokio::sync::{broadcast, watch};
use tp_contracts::bus::{Event, EventBus, EventKind, EventStream};
use tp_contracts::events::{
    CoreEvent, EventLog, EventPos, EventSink, LoggedEvent, Partition, PartitionAssigner,
    ShardEventLogs, ShardPartition, partition_of,
};
use tp_contracts::lease::{Lease, LeaseStore};
use tp_contracts::membership::NodeId;
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Error, Result, ShardId, TenantId};

/// Блокировка без паники: отравление мьютекса (паника в другом тесте) не ломает данные testkit.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `t + d` с насыщением (длительности из тестов могут быть сколь угодно большими).
pub(crate) fn add(t: OffsetDateTime, d: Duration) -> OffsetDateTime {
    time::Duration::try_from(d)
        .ok()
        .and_then(|d| t.checked_add(d))
        .unwrap_or(t)
}

// ---------- Часы ----------

/// Источник текущего времени (UTC).
pub trait Clock: fmt::Debug + Send + Sync + 'static {
    /// Текущий момент.
    fn now(&self) -> OffsetDateTime;
}

/// Системные часы.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// Часы под управлением теста; клоны разделяют одно время.
#[derive(Debug, Clone)]
pub struct ManualClock(Arc<Mutex<OffsetDateTime>>);

impl ManualClock {
    /// Часы, стоящие в `start`.
    pub fn new(start: OffsetDateTime) -> Self {
        Self(Arc::new(Mutex::new(start)))
    }

    /// Сдвигает время вперёд на `d`.
    pub fn advance(&self, d: Duration) {
        let mut now = lock(&self.0);
        *now = add(*now, d);
    }

    /// Устанавливает время.
    pub fn set(&self, t: OffsetDateTime) {
        *lock(&self.0) = t;
    }

    /// Часы как `Arc<dyn Clock>` для конструкторов testkit.
    pub fn shared(&self) -> Arc<dyn Clock> {
        Arc::new(self.clone())
    }
}

impl Default for ManualClock {
    /// 2026-01-01 00:00:00 UTC.
    fn default() -> Self {
        Self::new(
            OffsetDateTime::UNIX_EPOCH
                .replace_year(2026)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
        )
    }
}

impl Clock for ManualClock {
    fn now(&self) -> OffsetDateTime {
        *lock(&self.0)
    }
}

// ---------- Шина уведомлений ----------

/// Шина сигналов в памяти процесса (at-most-once, как реализации `tp-cluster` и `tp-meta`).
///
/// Отстающий подписчик теряет сигналы и получает вместо них один `EventKind::Resync`;
/// [`MemEventBus::disconnect`] завершает все текущие потоки — имитация потери связи для тестов переподписки.
#[derive(Debug, Clone)]
pub struct MemEventBus {
    tx: Arc<Mutex<broadcast::Sender<Event>>>,
    capacity: usize,
}

impl MemEventBus {
    /// Шина с буфером на `capacity` уведомлений на подписчика.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self {
            tx: Arc::new(Mutex::new(tx)),
            capacity: capacity.max(1),
        }
    }

    /// Завершает потоки всех текущих подписчиков; новые подписки работают.
    pub fn disconnect(&self) {
        let (tx, _) = broadcast::channel(self.capacity);
        *lock(&self.tx) = tx;
    }

    /// Число активных подписчиков.
    pub fn subscribers(&self) -> usize {
        lock(&self.tx).receiver_count()
    }

    /// Поток подписки; `resync` — сигнал сверки этой подписки.
    fn stream(
        &self,
        resync: Event,
        filter: impl Fn(&Event) -> bool + Send + 'static,
    ) -> EventStream {
        let rx = lock(&self.tx).subscribe();
        Box::pin(stream::unfold(
            (rx, filter, resync),
            |(mut rx, filter, resync)| async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) if filter(&ev) => return Some((ev, (rx, filter, resync))),
                        Ok(_) => {}
                        // Отставание: пропущенные сигналы потеряны (at-most-once) — подписчик сверяется.
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            return Some((resync.clone(), (rx, filter, resync)));
                        }
                        Err(broadcast::error::RecvError::Closed) => return None,
                    }
                }
            },
        ))
    }
}

impl Default for MemEventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[async_trait]
impl EventBus for MemEventBus {
    async fn publish(&self, event: Event) -> Result<()> {
        if event.kind == EventKind::Resync {
            return Err(Error::invalid(
                "Resync is inserted by the bus, not published",
            ));
        }
        // Нет подписчиков — сигнал никому не нужен.
        let _ = lock(&self.tx).send(event);
        Ok(())
    }

    async fn subscribe(&self, scope: AccountScope) -> Result<EventStream> {
        let resync = Event {
            tenant: scope.tenant,
            account: Some(scope.account),
            kind: EventKind::Resync,
        };
        Ok(self.stream(resync, move |e| {
            e.tenant == scope.tenant && e.account == Some(scope.account)
        }))
    }

    async fn subscribe_system(&self) -> Result<EventStream> {
        let resync = Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind: EventKind::Resync,
        };
        Ok(self.stream(resync, |e| e.account.is_none()))
    }
}

// ---------- Лизы ----------

#[derive(Debug, Default)]
struct LeaseState {
    held: HashMap<String, Lease>,
    /// Последний выданный номер захвата по имени; не сбрасывается при освобождении.
    fencing: HashMap<String, u64>,
}

/// Лизы в памяти; истечение — по [`Clock`].
#[derive(Debug, Clone)]
pub struct MemLease {
    state: Arc<Mutex<LeaseState>>,
    clock: Arc<dyn Clock>,
}

impl MemLease {
    /// Лизы с часами `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            state: Arc::new(Mutex::new(LeaseState::default())),
            clock,
        }
    }

    /// Текущий держатель лизы `name`, если она не истекла.
    pub fn holder(&self, name: &str) -> Option<String> {
        let now = self.clock.now();
        lock(&self.state)
            .held
            .get(name)
            .filter(|l| l.expires_at > now)
            .map(|l| l.holder.clone())
    }
}

impl Default for MemLease {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

#[async_trait]
impl LeaseStore for MemLease {
    async fn try_acquire(&self, name: &str, holder: &str, ttl: Duration) -> Result<Option<Lease>> {
        let now = self.clock.now();
        let expires_at = add(now, ttl);
        let mut st = lock(&self.state);
        if let Some(cur) = st.held.get_mut(name) {
            if cur.holder == holder {
                cur.expires_at = expires_at;
                return Ok(Some(cur.clone()));
            }
            if cur.expires_at > now {
                return Ok(None);
            }
        }
        let fencing = st.fencing.entry(name.to_owned()).or_default();
        *fencing += 1;
        let lease = Lease {
            name: name.to_owned(),
            holder: holder.to_owned(),
            fencing: *fencing,
            expires_at,
        };
        st.held.insert(name.to_owned(), lease.clone());
        Ok(Some(lease))
    }

    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<Option<Lease>> {
        let expires_at = add(self.clock.now(), ttl);
        let mut st = lock(&self.state);
        Ok(st
            .held
            .get_mut(&lease.name)
            .filter(|cur| cur.holder == lease.holder && cur.fencing == lease.fencing)
            .map(|cur| {
                cur.expires_at = expires_at;
                cur.clone()
            }))
    }

    async fn release(&self, lease: &Lease) -> Result<()> {
        let mut st = lock(&self.state);
        if st
            .held
            .get(&lease.name)
            .is_some_and(|cur| cur.holder == lease.holder && cur.fencing == lease.fencing)
        {
            st.held.remove(&lease.name);
        }
        Ok(())
    }
}

// ---------- Журнал событий ядра ----------

#[derive(Debug, Default)]
struct LogState {
    partitions: BTreeMap<Partition, Vec<LoggedEvent>>,
    positions: HashMap<(String, Partition), EventPos>,
}

/// Журнал событий ядра в памяти. События добавляет [`MemEventLog::append`] (в `tp-meta` — запись
/// в транзакции изменения; в testkit — `MemMetadata` и тесты). Позиции подписчиков переживают
/// пересоздание подписчика (рестарт): они хранятся в журнале, как в PostgreSQL.
#[derive(Debug, Clone)]
pub struct MemEventLog {
    state: Arc<Mutex<LogState>>,
    partitions: u32,
}

impl MemEventLog {
    /// Журнал из `partitions` разделов (не меньше одного).
    pub fn new(partitions: u32) -> Self {
        Self {
            state: Arc::new(Mutex::new(LogState::default())),
            partitions: partitions.max(1),
        }
    }

    /// Добавляет событие в раздел его учётной записи.
    pub fn append(&self, event: CoreEvent) -> LoggedEvent {
        let partition = partition_of(event.account, self.partitions);
        let mut st = lock(&self.state);
        let part = st.partitions.entry(partition).or_default();
        let pos = EventPos(part.last().map_or(0, |e| e.pos.0) + 1);
        let logged = LoggedEvent {
            partition,
            pos,
            event,
        };
        part.push(logged.clone());
        logged
    }

    /// Все события всех разделов (раздел, затем позиция).
    pub fn all(&self) -> Vec<LoggedEvent> {
        lock(&self.state)
            .partitions
            .values()
            .flatten()
            .cloned()
            .collect()
    }

    /// Один проход подписчика по всем разделам: читает после зафиксированной позиции пачками
    /// по `limit`, передаёт `sink` и фиксирует. Ошибка `sink` — позиция раздела не сдвигается.
    /// Возвращает число обработанных событий.
    pub async fn drain(&self, sink: &dyn EventSink, limit: usize) -> Result<usize> {
        let mut total = 0;
        for p in 0..self.partitions {
            let partition = Partition(p);
            loop {
                let after = self.position(sink.name(), partition).await?;
                let batch = self.read(partition, after, limit).await?;
                let Some(last) = batch.last().map(|e| e.pos) else {
                    break;
                };
                sink.handle(&batch).await?;
                self.commit(sink.name(), partition, last).await?;
                total += batch.len();
            }
        }
        Ok(total)
    }

    fn check(&self, partition: Partition) -> Result<()> {
        if partition.0 >= self.partitions {
            return Err(Error::invalid(format!(
                "partition {} out of {}",
                partition.0, self.partitions
            )));
        }
        Ok(())
    }
}

impl Default for MemEventLog {
    fn default() -> Self {
        Self::new(4)
    }
}

#[async_trait]
impl EventLog for MemEventLog {
    fn partitions(&self) -> u32 {
        self.partitions
    }

    async fn read(
        &self,
        partition: Partition,
        after: EventPos,
        limit: usize,
    ) -> Result<Vec<LoggedEvent>> {
        self.check(partition)?;
        let st = lock(&self.state);
        Ok(st
            .partitions
            .get(&partition)
            .map(|events| {
                let start = events.partition_point(|e| e.pos <= after);
                events[start..].iter().take(limit).cloned().collect()
            })
            .unwrap_or_default())
    }

    async fn position(&self, subscriber: &str, partition: Partition) -> Result<EventPos> {
        self.check(partition)?;
        Ok(lock(&self.state)
            .positions
            .get(&(subscriber.to_owned(), partition))
            .copied()
            .unwrap_or_default())
    }

    async fn commit(&self, subscriber: &str, partition: Partition, pos: EventPos) -> Result<()> {
        self.check(partition)?;
        let mut st = lock(&self.state);
        let cur = st
            .positions
            .entry((subscriber.to_owned(), partition))
            .or_default();
        *cur = (*cur).max(pos);
        Ok(())
    }
}

// ---------- Журнал событий по шардам ----------

#[derive(Debug, Default)]
struct ShardsState {
    logs: BTreeMap<ShardId, MemEventLog>,
    placement: HashMap<AccountId, ShardId>,
}

/// Журналы событий ядра нескольких шардов в памяти (CTR-34): у каждого шарда — свой [`MemEventLog`]
/// с `partitions` разделами и своими позициями подписчиков. Справочник шардов — [`MemShardedEventLog::place`]
/// (учётная запись без размещения — на [`ShardId::MAIN`]). Шард [`ShardId::MAIN`] есть всегда.
#[derive(Debug, Clone)]
pub struct MemShardedEventLog {
    state: Arc<Mutex<ShardsState>>,
    partitions: u32,
}

impl MemShardedEventLog {
    /// Журналы шардов `shards` (и [`ShardId::MAIN`]) по `partitions` разделов (не меньше одного).
    pub fn new(shards: &[ShardId], partitions: u32) -> Self {
        let partitions = partitions.max(1);
        let logs = shards
            .iter()
            .copied()
            .chain([ShardId::MAIN])
            .map(|s| (s, MemEventLog::new(partitions)))
            .collect();
        Self {
            state: Arc::new(Mutex::new(ShardsState {
                logs,
                placement: HashMap::new(),
            })),
            partitions,
        }
    }

    /// Добавляет шард в справочник (повтор — без изменений).
    pub fn add_shard(&self, shard: ShardId) {
        let partitions = self.partitions;
        lock(&self.state)
            .logs
            .entry(shard)
            .or_insert_with(|| MemEventLog::new(partitions));
    }

    /// Размещает учётную запись на шарде (справочник); шард вне справочника — `Invalid`.
    pub fn place(&self, account: AccountId, shard: ShardId) -> Result<()> {
        let mut st = lock(&self.state);
        if !st.logs.contains_key(&shard) {
            return Err(Error::invalid(format!(
                "shard {shard} is not in the directory"
            )));
        }
        st.placement.insert(account, shard);
        Ok(())
    }

    /// Шард учётной записи по справочнику.
    pub fn shard_of(&self, account: AccountId) -> ShardId {
        lock(&self.state)
            .placement
            .get(&account)
            .copied()
            .unwrap_or(ShardId::MAIN)
    }

    /// Журнал шарда.
    pub fn shard_log(&self, shard: ShardId) -> Option<MemEventLog> {
        lock(&self.state).logs.get(&shard).cloned()
    }

    /// Добавляет событие в журнал шарда его учётной записи; возвращает раздел инсталляции и запись.
    pub fn append(&self, event: CoreEvent) -> (ShardPartition, LoggedEvent) {
        let (shard, log) = {
            let st = lock(&self.state);
            let shard = st
                .placement
                .get(&event.account)
                .copied()
                .unwrap_or(ShardId::MAIN);
            let log = st
                .logs
                .get(&shard)
                .cloned()
                .unwrap_or_else(|| MemEventLog::new(self.partitions));
            (shard, log)
        };
        let logged = log.append(event);
        (ShardPartition::new(shard, logged.partition), logged)
    }

    /// Один проход подписчика по всем разделам всех шардов (по возрастанию шарда) — [`MemEventLog::drain`].
    pub async fn drain(&self, sink: &dyn EventSink, limit: usize) -> Result<usize> {
        let logs: Vec<MemEventLog> = lock(&self.state).logs.values().cloned().collect();
        let mut total = 0;
        for log in logs {
            total += log.drain(sink, limit).await?;
        }
        Ok(total)
    }
}

impl Default for MemShardedEventLog {
    fn default() -> Self {
        Self::new(&[], 4)
    }
}

impl ShardEventLogs for MemShardedEventLog {
    fn shards(&self) -> Vec<ShardId> {
        lock(&self.state).logs.keys().copied().collect()
    }

    fn log(&self, shard: ShardId) -> Option<Arc<dyn EventLog>> {
        self.shard_log(shard)
            .map(|l| Arc::new(l) as Arc<dyn EventLog>)
    }

    fn partitions(&self) -> u32 {
        self.partitions
    }
}

// ---------- Назначение разделов ----------

/// FNV-1a 64 по байтам: стабильный вес rendezvous-хэширования.
fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in *part {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Назначение разделов журнала в памяти (CONTRACTS.md#Events): rendezvous-хэширование ключа
/// `<подписчик>/<раздел>` по составу [`MemPartitionAssigner::set_members`] (в `tp-cluster` — по `Membership`).
/// Пары (шард, раздел) — метод по умолчанию [`PartitionAssigner::assigned_sharded`].
#[derive(Debug, Clone)]
pub struct MemPartitionAssigner {
    members: Arc<watch::Sender<Vec<NodeId>>>,
}

impl MemPartitionAssigner {
    /// Назначение для состава `members`.
    pub fn new(members: Vec<NodeId>) -> Self {
        let mut members = members;
        members.sort();
        members.dedup();
        Self {
            members: Arc::new(watch::Sender::new(members)),
        }
    }

    /// Меняет состав (подписчики [`PartitionAssigner::members`] получают новое значение).
    pub fn set_members(&self, members: Vec<NodeId>) {
        let mut members = members;
        members.sort();
        members.dedup();
        self.members.send_replace(members);
    }
}

impl PartitionAssigner for MemPartitionAssigner {
    fn assigned(&self, subscriber: &str, node: &NodeId, partitions: u32) -> Vec<Partition> {
        let members = self.members.borrow();
        if !members.contains(node) {
            return Vec::new();
        }
        (0..partitions)
            .map(Partition)
            .filter(|p| {
                let key = format!("{subscriber}/{p}");
                let owner = members
                    .iter()
                    .max_by_key(|m| (fnv1a(&[key.as_bytes(), m.as_str().as_bytes()]), *m));
                owner == Some(node)
            })
            .collect()
    }

    fn members(&self) -> watch::Receiver<Vec<NodeId>> {
        self.members.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tp_contracts::bus::EventKind;
    use tp_contracts::events::CoreEventKind;
    use tp_contracts::{AccountId, MailboxId, MessageId, ModSeq, TenantId};

    const TTL: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn lease_expires_by_manual_clock() {
        let clock = ManualClock::default();
        let leases = MemLease::new(clock.shared());
        let a = leases
            .try_acquire("leader", "a", TTL)
            .await
            .expect("ok")
            .expect("ok");
        assert_eq!(a.expires_at, clock.now() + TTL);
        assert!(
            leases
                .try_acquire("leader", "b", TTL)
                .await
                .expect("ok")
                .is_none()
        );

        // Тот же держатель продлевает без смены номера захвата.
        clock.advance(Duration::from_secs(10));
        let a2 = leases
            .try_acquire("leader", "a", TTL)
            .await
            .expect("ok")
            .expect("ok");
        assert_eq!(a2.fencing, a.fencing);
        assert_eq!(a2.expires_at, clock.now() + TTL);

        clock.advance(TTL - Duration::from_secs(1));
        assert!(
            leases
                .try_acquire("leader", "b", TTL)
                .await
                .expect("ok")
                .is_none()
        );
        clock.advance(Duration::from_secs(1));
        assert_eq!(leases.holder("leader"), None);
        let b = leases
            .try_acquire("leader", "b", TTL)
            .await
            .expect("ok")
            .expect("ok");
        assert!(b.fencing > a.fencing);

        // Прежний держатель потерял лизу: продление и освобождение на неё не действуют.
        assert!(leases.renew(&a2, TTL).await.expect("ok").is_none());
        leases.release(&a2).await.expect("ok");
        assert_eq!(leases.holder("leader").as_deref(), Some("b"));

        let b2 = leases.renew(&b, TTL).await.expect("ok").expect("ok");
        assert_eq!(b2.fencing, b.fencing);
        leases.release(&b2).await.expect("ok");
        let c = leases
            .try_acquire("leader", "c", TTL)
            .await
            .expect("ok")
            .expect("ok");
        assert!(
            c.fencing > b.fencing,
            "номер захвата растёт и после освобождения"
        );
    }

    #[tokio::test]
    async fn expired_lease_renewable_until_taken() {
        let clock = ManualClock::default();
        let leases = MemLease::new(clock.shared());
        let a = leases
            .try_acquire("idx", "a", TTL)
            .await
            .expect("ok")
            .expect("ok");
        clock.advance(TTL * 2);
        let a2 = leases.renew(&a, TTL).await.expect("ok").expect("ok");
        assert_eq!(a2.expires_at, clock.now() + TTL);
    }

    fn core(account: AccountId, mailbox: MailboxId) -> CoreEvent {
        CoreEvent {
            tenant: TenantId::DEFAULT,
            account,
            mailbox: Some(mailbox),
            kind: CoreEventKind::Delivered {
                message: MessageId::new(),
            },
            at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[derive(Debug)]
    struct Recorder {
        seen: Mutex<Vec<LoggedEvent>>,
        fail: Mutex<bool>,
    }

    #[async_trait]
    impl EventSink for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }

        async fn handle(&self, batch: &[LoggedEvent]) -> Result<()> {
            if *lock(&self.fail) {
                return Err(Error::UNAVAILABLE);
            }
            lock(&self.seen).extend_from_slice(batch);
            Ok(())
        }
    }

    fn recorder() -> Recorder {
        Recorder {
            seen: Mutex::new(Vec::new()),
            fail: Mutex::new(false),
        }
    }

    #[tokio::test]
    async fn subscriber_resumes_after_restart() {
        let log = MemEventLog::new(4);
        let (acc, mbx) = (AccountId::new(), MailboxId::new());
        for _ in 0..5 {
            log.append(core(acc, mbx));
        }
        let first = recorder();
        assert_eq!(log.drain(&first, 2).await.expect("ok"), 5);

        for _ in 0..3 {
            log.append(core(acc, mbx));
        }
        // Рестарт: новый экземпляр с тем же именем продолжает с сохранённой позиции.
        let second = recorder();
        assert_eq!(log.drain(&second, 10).await.expect("ok"), 3);
        let positions: Vec<_> = lock(&second.seen).iter().map(|e| e.pos.0).collect();
        assert_eq!(positions, [6, 7, 8]);

        // Ошибка обработки: позиция не сдвигается, события придут снова.
        log.append(core(acc, mbx));
        *lock(&second.fail) = true;
        assert!(log.drain(&second, 10).await.is_err());
        *lock(&second.fail) = false;
        assert_eq!(log.drain(&second, 10).await.expect("ok"), 1);

        // Позиция не уменьшается.
        let p = partition_of(acc, 4);
        log.commit("recorder", p, EventPos(1)).await.expect("ok");
        assert_eq!(log.position("recorder", p).await.expect("ok"), EventPos(9));
        assert!(log.read(Partition(4), EventPos(0), 1).await.is_err());
    }

    #[tokio::test]
    async fn shards_keep_separate_logs_and_positions() {
        let logs = MemShardedEventLog::new(&[ShardId(2), ShardId(1)], 3);
        assert_eq!(logs.shards(), [ShardId::MAIN, ShardId(1), ShardId(2)]);
        crate::suite::shard_event_logs(&logs).await;
        let (a0, a2) = (AccountId::new(), AccountId::new());
        logs.place(a2, ShardId(2)).expect("place");
        assert!(logs.place(a0, ShardId(7)).is_err());
        assert_eq!(logs.shard_of(a0), ShardId::MAIN);
        let (p0, e0) = logs.append(core(a0, MailboxId::new()));
        let (p2, e2) = logs.append(core(a2, MailboxId::new()));
        assert_eq!(p0, ShardPartition::of(ShardId::MAIN, a0, 3));
        assert_eq!(p2, ShardPartition::of(ShardId(2), a2, 3));
        // Позиции — свои у каждого шарда.
        assert_eq!((e0.pos, e2.pos), (EventPos(1), EventPos(1)));
        let rec = recorder();
        assert_eq!(logs.drain(&rec, 10).await.expect("ok"), 2);
        let log2 = logs.log(ShardId(2)).expect("shard 2");
        assert_eq!(
            log2.position("recorder", p2.index).await.expect("ok"),
            EventPos(1)
        );
        assert!(logs.log(ShardId(9)).is_none());
        logs.add_shard(ShardId(9));
        assert!(logs.log(ShardId(9)).is_some());
        assert_eq!(logs.drain(&rec, 10).await.expect("ok"), 0);
    }

    #[test]
    fn mem_assigner_conformance() {
        let node = |n: &str| NodeId::new(n).expect("node");
        let a = MemPartitionAssigner::new(vec![node("n1")]);
        let rx = a.members();
        crate::suite::partition_assigner(&a, &[ShardId::MAIN], 8);
        crate::suite::partition_assigner(&a, &[ShardId(1), ShardId(2)], 8);
        a.set_members(vec![node("n3"), node("n1"), node("n2")]);
        assert!(rx.has_changed().expect("open"));
        crate::suite::partition_assigner(&a, &[ShardId::MAIN], 16);
        crate::suite::partition_assigner(&a, &[ShardId(1), ShardId(5), ShardId(1)], 16);
    }

    #[tokio::test]
    async fn order_within_mailbox() {
        let log = MemEventLog::new(3);
        let accounts: Vec<_> = (0..6)
            .map(|_| (AccountId::new(), MailboxId::new()))
            .collect();
        for round in 0..20 {
            let (acc, mbx) = accounts[round % accounts.len()];
            log.append(core(acc, mbx));
        }
        let rec = recorder();
        assert_eq!(log.drain(&rec, 3).await.expect("ok"), 20);
        let seen = lock(&rec.seen);
        for (acc, mbx) in &accounts {
            let mine: Vec<_> = seen
                .iter()
                .filter(|e| e.event.mailbox == Some(*mbx))
                .collect();
            assert!(mine.iter().all(|e| e.partition == partition_of(*acc, 3)));
            assert!(mine.windows(2).all(|w| w[0].pos < w[1].pos));
        }
    }

    #[tokio::test]
    async fn bus_routes_and_disconnects() {
        let bus = MemEventBus::default();
        let scope = AccountScope {
            tenant: TenantId::DEFAULT,
            account: AccountId::new(),
        };
        let mut mine = bus.subscribe(scope).await.expect("ok");
        let mut system = bus.subscribe_system().await.expect("ok");
        let mailbox = MailboxId::new();
        let other = Event {
            tenant: TenantId::DEFAULT,
            account: Some(AccountId::new()),
            kind: EventKind::MailboxListChanged,
        };
        let changed = Event {
            tenant: scope.tenant,
            account: Some(scope.account),
            kind: EventKind::MailboxChanged {
                mailbox,
                modseq: ModSeq(3),
            },
        };
        let queue = Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind: EventKind::QueueReady,
        };
        for e in [other, changed.clone(), queue.clone()] {
            bus.publish(e).await.expect("ok");
        }
        assert_eq!(mine.next().await, Some(changed));
        assert_eq!(system.next().await, Some(queue));

        assert_eq!(bus.subscribers(), 2);
        bus.disconnect();
        assert_eq!(mine.next().await, None);
        assert_eq!(system.next().await, None);
        let mut again = bus.subscribe_system().await.expect("ok");
        bus.publish(Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind: EventKind::ConfigReloaded,
        })
        .await
        .expect("ok");
        assert!(again.next().await.is_some());
    }

    #[tokio::test]
    async fn lagging_subscriber_gets_resync() {
        let bus = MemEventBus::new(2);
        let mut sys = bus.subscribe_system().await.expect("ok");
        let queue = Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind: EventKind::QueueReady,
        };
        for _ in 0..5 {
            bus.publish(queue.clone()).await.expect("ok");
        }
        bus.disconnect();
        let got: Vec<Event> = sys.by_ref().collect().await;
        let resync = Event {
            kind: EventKind::Resync,
            ..queue.clone()
        };
        assert_eq!(got, vec![resync, queue.clone(), queue]);
    }

    #[tokio::test]
    async fn passes_signal_suite() {
        crate::suite::signals(&MemEventBus::default()).await;
    }
}
