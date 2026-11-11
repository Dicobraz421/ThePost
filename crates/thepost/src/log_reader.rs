//! Чтение журнала событий без опроса (APP-08; SRS 2.8, решение 2026-11-01-interaction-db-ha): читатель ждёт
//! сигнала и дочитывает разделы до конца.
//!
//! - `LogAppended { partition }` — дочитать этот раздел (сигналы, накопившиеся в потоке, — одной пачкой);
//! - при запуске, после (пере)подписки на сигналы, по `Resync`, `MembersChanged` и смене состава
//!   (`PartitionAssigner::members`) — сверка: дочитать все назначенные разделы (`EventConsumer::poll`);
//! - таймеры — только из закрытого списка SRS 2.8: срок повтора раздела с ошибкой (`PollReport::retry_in`),
//!   истечение чужой лизы назначенного разделу узлу (`meta.events.lease_ttl`), повтор подписки на сигналы после
//!   ошибки (`meta.events.retry_min`).
//!
//! Раздел с ошибкой до срока повтора читается только сверкой (`poll` соблюдает паузу повтора), а не по каждому
//! его сигналу.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use futures::{FutureExt, StreamExt};
use tokio::sync::watch;
use tokio::time::Instant;
use tp_contracts::Result;
use tp_contracts::bus::{EventBus, EventKind, EventStream};
use tp_contracts::events::{EventSink, Partition};
use tp_contracts::membership::NodeId;
use tp_contracts::role::CancellationToken;
use tp_meta::events::EventConsumer;

/// Источники пробуждения читателей журнала узла.
#[derive(Clone)]
pub struct LogSignals {
    /// Шина сигналов узла (системная подписка).
    pub bus: Arc<dyn EventBus>,
    /// Состав кластера распределителя разделов; `None` — один узел, все разделы его.
    pub members: Option<watch::Receiver<Vec<NodeId>>>,
    /// Срок лизы раздела (`meta.events.lease_ttl`): через столько чужая лиза назначенного раздела истекает.
    pub lease_ttl: Duration,
    /// Пауза перед повтором подписки на сигналы после ошибки (`meta.events.retry_min`).
    pub resubscribe: Duration,
}

impl std::fmt::Debug for LogSignals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogSignals")
            .field("members", &self.members.is_some())
            .field("lease_ttl", &self.lease_ttl)
            .field("resubscribe", &self.resubscribe)
            .finish_non_exhaustive()
    }
}

/// Что дочитать.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    /// Сверка: все назначенные разделы.
    All,
    /// Разделы из сигналов `LogAppended`.
    Partitions(BTreeSet<Partition>),
}

/// Итог ожидания в потоке сигналов.
enum Got {
    Stop,
    Due,
    Members(bool),
    Signal(Option<EventKind>),
}

/// Читатель журнала событий узла: ожидание повода ([`LogReader::wait`]) и чтение разделов подписчика
/// ([`LogReader::read`]).
pub struct LogReader {
    events: EventConsumer,
    signals: LogSignals,
    stream: Option<EventStream>,
    /// Нужна сверка.
    full: bool,
    /// Срок ближайшей сверки по таймеру: повтор раздела с ошибкой или истечение чужой лизы.
    due: Option<Instant>,
    /// Разделы подписчиков с ошибкой (до срока повтора — только сверкой).
    backoff: HashMap<String, BTreeSet<Partition>>,
}

impl std::fmt::Debug for LogReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogReader")
            .field("subscribed", &self.stream.is_some())
            .field("full", &self.full)
            .field("due", &self.due)
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

impl LogReader {
    /// Читатель над `events`; первое [`LogReader::wait`] — сверка.
    pub fn new(events: EventConsumer, signals: LogSignals) -> Self {
        Self {
            events,
            signals,
            stream: None,
            full: true,
            due: None,
            backoff: HashMap::new(),
        }
    }

    /// Ждёт повода прочитать журнал; `None` — остановка.
    pub async fn wait(&mut self, shutdown: &CancellationToken) -> Option<Work> {
        loop {
            if shutdown.is_cancelled() {
                return None;
            }
            if self.stream.is_none() {
                match self.signals.bus.subscribe_system().await {
                    // Сверка после подписки: сигналы до неё потеряны (SRS 2.8, правило 3).
                    Ok(s) => {
                        self.stream = Some(s);
                        self.full = true;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, retry_in = ?self.signals.resubscribe,
                            "журнал событий: нет подписки на сигналы, повтор по сроку");
                        if self.full {
                            break;
                        }
                        let retry = Instant::now() + self.signals.resubscribe;
                        let at = self.due.map_or(retry, |d| d.min(retry));
                        tokio::select! {
                            () = shutdown.cancelled() => return None,
                            () = tokio::time::sleep_until(at) => {}
                        }
                        // Без сигналов — сверка к сроку повтора подписки.
                        self.full = true;
                        continue;
                    }
                }
            }
            if self.full {
                break;
            }
            let got = self.next_signal(shutdown).await;
            match got {
                Got::Stop => return None,
                Got::Due => self.full = true,
                Got::Members(alive) => {
                    if alive {
                        self.full = true;
                    } else {
                        self.signals.members = None;
                    }
                }
                Got::Signal(None) => self.stream = None,
                Got::Signal(Some(EventKind::LogAppended { partition })) => {
                    let mut parts = BTreeSet::from([partition]);
                    // Сигналы, уже накопившиеся в потоке, — одной пачкой.
                    while let Some(stream) = self.stream.as_mut() {
                        match stream.next().now_or_never() {
                            Some(Some(e)) => match e.kind {
                                EventKind::LogAppended { partition } => {
                                    parts.insert(partition);
                                }
                                EventKind::Resync | EventKind::MembersChanged => self.full = true,
                                _ => {}
                            },
                            Some(None) => self.stream = None,
                            None => break,
                        }
                    }
                    if !self.full {
                        return Some(Work::Partitions(parts));
                    }
                }
                Got::Signal(Some(EventKind::Resync | EventKind::MembersChanged)) => {
                    self.full = true;
                }
                Got::Signal(Some(_)) => {}
            }
        }
        self.full = false;
        self.due = None;
        Some(Work::All)
    }

    async fn next_signal(&mut self, shutdown: &CancellationToken) -> Got {
        let Some(stream) = self.stream.as_mut() else {
            return Got::Signal(None);
        };
        let due = self.due;
        let members = self.signals.members.as_mut();
        tokio::select! {
            () = shutdown.cancelled() => Got::Stop,
            () = sleep_until(due) => Got::Due,
            alive = changed(members) => Got::Members(alive),
            e = stream.next() => Got::Signal(e.map(|e| e.kind)),
        }
    }

    /// Дочитывает до конца разделы `work` подписчика `sink`. Ошибка раздела — в журнал узла, раздел — к повтору
    /// по сроку; ошибка — только ошибка прохода сверки.
    pub async fn read(&mut self, sink: &dyn EventSink, work: &Work) -> Result<()> {
        let Work::Partitions(parts) = work else {
            return self.read_all(sink).await;
        };
        let name = sink.name().to_owned();
        if self
            .backoff
            .get(&name)
            .is_some_and(|b| parts.iter().any(|p| b.contains(p)))
        {
            return self.read_all(sink).await;
        }
        let assigned = self.events.assigned(&name);
        for p in parts {
            if assigned.binary_search(p).is_err() {
                continue;
            }
            loop {
                match self.events.poll_partition(sink, *p).await {
                    Ok(Some(0)) => break,
                    Ok(Some(_)) => {}
                    // Лизу держит другой экземпляр: раздел станет доступен по её истечении.
                    Ok(None) => {
                        self.due_in(self.signals.lease_ttl);
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(subscriber = %name, partition = p.0, error = %e,
                            "журнал событий: раздел не обработан, повтор по сроку");
                        // Сверка учтёт ошибку и назначит срок повтора раздела.
                        return self.read_all(sink).await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Сверка: проходы по всем назначенным разделам, пока есть необработанные события.
    async fn read_all(&mut self, sink: &dyn EventSink) -> Result<()> {
        let name = sink.name().to_owned();
        loop {
            let report = self.events.poll(sink).await?;
            let backoff = self.backoff.entry(name.clone()).or_default();
            if report.retry_in.is_none() {
                backoff.clear();
            }
            backoff.extend(report.failed.iter().map(|(p, _)| *p));
            if let Some(r) = report.retry_in {
                self.due_in(r);
            }
            if report.handled == 0 {
                break;
            }
        }
        let held = self.events.held(&name);
        let backoff = self.backoff.get(&name);
        let busy =
            self.events.assigned(&name).into_iter().any(|p| {
                held.binary_search(&p).is_err() && backoff.is_none_or(|b| !b.contains(&p))
            });
        if busy {
            self.due_in(self.signals.lease_ttl);
        }
        Ok(())
    }

    /// Сверка не позже чем через `after` (повтор при временной ошибке вызывающего).
    pub fn wake_in(&mut self, after: Duration) {
        self.due_in(after);
    }

    fn due_in(&mut self, after: Duration) {
        let at = Instant::now() + after;
        self.due = Some(self.due.map_or(at, |d| d.min(at)));
    }

    /// Отдаёт лизы разделов подписчика (штатная остановка).
    pub async fn release(&self, sink: &dyn EventSink) {
        if let Err(e) = self.events.release_all(sink.name()).await {
            tracing::warn!(subscriber = sink.name(), error = %e, "журнал событий: лизы разделов не отданы");
        }
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Состав изменился (`true`) или источник закрыт (`false`); нет источника — не завершается.
async fn changed(members: Option<&mut watch::Receiver<Vec<NodeId>>>) -> bool {
    match members {
        Some(rx) => rx.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tp_cluster::{LocalBus, LocalBusConfig};
    use tp_contracts::bus::Event;
    use tp_contracts::events::{CoreEvent, CoreEventKind, EventLog, LoggedEvent};
    use tp_contracts::{AccountId, Error, TenantId};
    use tp_testkit::{Clock, MemEventLog, MemLease, SystemClock};

    use super::*;

    /// Подписчик: обработанные события; `fail` — отказ обработки.
    #[derive(Default)]
    struct Sink {
        seen: Mutex<Vec<u64>>,
        fail: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl EventSink for Sink {
        fn name(&self) -> &str {
            "test"
        }

        async fn handle(&self, batch: &[LoggedEvent]) -> Result<()> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::internal("отказ"));
            }
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(batch.iter().map(|e| e.pos.0));
            Ok(())
        }
    }

    impl Sink {
        fn count(&self) -> usize {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        }
    }

    fn event() -> CoreEvent {
        CoreEvent {
            tenant: TenantId::DEFAULT,
            account: AccountId::new(),
            mailbox: None,
            kind: CoreEventKind::MailboxCreated,
            at: SystemClock.now(),
        }
    }

    fn signal(kind: EventKind) -> Event {
        Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind,
        }
    }

    /// Журнал из 2 разделов, шина в процессе, срок лизы и пауза повтора — час: таймеры в тесте не срабатывают.
    fn setup(batch: usize) -> (MemEventLog, LocalBus, LogReader) {
        let log = MemEventLog::new(2);
        let bus = LocalBus::new(&LocalBusConfig::default());
        let events = EventConsumer::new(
            Arc::new(log.clone()) as Arc<dyn EventLog>,
            Arc::new(MemLease::default()),
            "node-a/1",
            tp_meta::events::EventsConfig {
                batch,
                retry_min: Duration::from_secs(3600),
                retry_max: Duration::from_secs(3600),
                ..tp_meta::events::EventsConfig::default()
            },
        );
        let reader = LogReader::new(
            events,
            LogSignals {
                bus: Arc::new(bus.clone()),
                members: None,
                lease_ttl: Duration::from_secs(3600),
                resubscribe: Duration::from_secs(3600),
            },
        );
        (log, bus, reader)
    }

    async fn wait(reader: &mut LogReader) -> Work {
        let stop = CancellationToken::new();
        tokio::time::timeout(Duration::from_secs(5), reader.wait(&stop))
            .await
            .expect("повод не пришёл за 5 с")
            .expect("работа")
    }

    /// Приёмка APP-08: событие журнала обрабатывается по сигналу `LogAppended`, без ожидания периода опроса;
    /// раздел дочитывается до конца (несколько пачек).
    #[tokio::test]
    async fn log_appended_wakes_reader_without_polling() {
        let (log, bus, mut reader) = setup(2);
        let sink = Sink::default();
        // Запуск — сверка.
        assert_eq!(wait(&mut reader).await, Work::All);
        reader.read(&sink, &Work::All).await.expect("read");
        assert_eq!(sink.count(), 0);

        // Пять событий одного раздела (пачка — 2) и сигнал: читатель просыпается и дочитывает раздел.
        let account = AccountId::new();
        let mut partition = None;
        for _ in 0..5 {
            let e = log.append(CoreEvent { account, ..event() });
            partition = Some(e.partition);
        }
        let partition = partition.expect("partition");
        bus.publish(signal(EventKind::LogAppended { partition }))
            .await
            .expect("publish");
        let work = wait(&mut reader).await;
        assert_eq!(work, Work::Partitions(BTreeSet::from([partition])));
        reader.read(&sink, &work).await.expect("read");
        assert_eq!(sink.count(), 5);

        // Без сигнала ожидание не завершается: опроса нет.
        log.append(event());
        let stop = CancellationToken::new();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), reader.wait(&stop))
                .await
                .is_err(),
            "читатель проснулся без сигнала"
        );
    }

    /// `Resync` — сверка всех разделов; сигналы, накопившиеся в потоке, — одной пачкой.
    #[tokio::test]
    async fn resync_and_batched_signals() {
        let (log, bus, mut reader) = setup(100);
        let sink = Sink::default();
        assert_eq!(wait(&mut reader).await, Work::All);
        let a = log.append(event());
        let b = loop {
            let e = log.append(event());
            if e.partition != a.partition {
                break e;
            }
        };
        for p in [a.partition, b.partition, a.partition] {
            bus.publish(signal(EventKind::LogAppended { partition: p }))
                .await
                .expect("publish");
        }
        let work = wait(&mut reader).await;
        assert_eq!(
            work,
            Work::Partitions(BTreeSet::from([a.partition, b.partition]))
        );
        reader.read(&sink, &work).await.expect("read");
        assert!(sink.count() >= 2);

        bus.publish(signal(EventKind::MembersChanged))
            .await
            .expect("publish");
        assert_eq!(wait(&mut reader).await, Work::All);
    }

    /// Ошибка подписчика: раздел — к повтору по сроку (сверкой), его сигналы до срока — тоже сверкой.
    #[tokio::test]
    async fn failed_partition_waits_for_retry() {
        let (log, bus, mut reader) = setup(100);
        let sink = Sink::default();
        assert_eq!(wait(&mut reader).await, Work::All);
        let e = log.append(event());
        sink.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let work = Work::Partitions(BTreeSet::from([e.partition]));
        reader.read(&sink, &work).await.expect("read");
        assert!(reader.due.is_some(), "срок повтора назначен");
        assert!(
            reader
                .backoff
                .get("test")
                .is_some_and(|b| b.contains(&e.partition)),
            "{reader:?}"
        );
        sink.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        bus.publish(signal(EventKind::LogAppended {
            partition: e.partition,
        }))
        .await
        .expect("publish");
        let work = wait(&mut reader).await;
        // До срока повтора раздел пропускается сверкой (`deferred`).
        reader.read(&sink, &work).await.expect("read");
        assert_eq!(sink.count(), 0);
    }
}
