//! Сигналы кластера через PostgreSQL LISTEN/NOTIFY (META-26; CONTRACTS.md#EventBus, SRS 2.8,
//! решение 2026-11-01 «Взаимодействие компонентов»).
//!
//! - Публикатор узла: [`PgSignalBus::publish`] только ставит сигнал в очередь (вызывающий уже зафиксировал
//!   изменение); задача публикатора отправляет NOTIFY отдельной короткой транзакцией. Пока отправка не
//!   завершена, новые сигналы копятся и уходят одной пачкой следующей отправкой — склейка зависит от нагрузки,
//!   таймера нет. Одинаковые сигналы пачки склеиваются в один; пачка делится на сообщения NOTIFY не длиннее
//!   [`SignalsConfig::max_payload`]. Очередь длиннее [`SignalsConfig::max_pending`] — сигналы отбрасываются, а
//!   все подписчики кластера получают `Resync`.
//! - Слушатель узла: одно прямое соединение с primary мимо PgBouncer ([`connect_listener`]); primary ищется
//!   так же, как для пула узла ([`crate::failover`], META-25). После переподключения (обрыв, ошибка соединения,
//!   переключение primary пула узла) все подписчики узла получают `Resync` и сверяются с хранилищем.
//! - Фильтрация по интересам — на узле: подписка учётной записи получает только свои сигналы, системная —
//!   системные сигналы всех организаций. У подписчика ограниченный буфер ([`SignalsConfig::subscriber_buffer`]):
//!   при переполнении накопленные сигналы заменяются одним `Resync`.
//!
//! Контракт [`EventBus`] (META-36): подписка — поток [`EventStream`] над [`Subscription`]; поток завершается при
//! остановке шины, `drop` потока удаляет подписку.
//!
//! Метрики — [`PgSignalBus::metrics`]: задержка сигнала (от постановки в очередь до приёма слушателем, по
//! часам узлов), размер пачек, время NOTIFY; для критериев Д.7 (META-53) — гистограммы времени фиксаций с NOTIFY
//! и всех фиксаций процесса ([`crate::commits`]), их доля ([`SignalMetrics::notify_commit_share`]) и число
//! соединений-слушателей узла ([`SignalMetrics::listeners`]; по кластеру — сумма по узлам).
//!
//! Шарды (META-31, решение 2026-11-01-scale-shards-geo-admin): сигнал шарда разделяет судьбу его данных —
//! [`ShardedSignalBus`] держит шину [`PgSignalBus`] на primary каждого шарда справочника (слушатель и
//! публикатор — прямые соединения с шардом); сигнал учётной записи публикуется на её шарде, системный — в
//! основной БД; подписки получают сигналы всех шардов.
//!
//! Формат сообщения NOTIFY — JSON `{"t": <микросекунды UTC самого раннего сигнала>, "r": <сверка всем>,
//! "e": [<Event>…]}`.
//!
//! ```yaml
//! meta:
//!   signals: { channel: tp_signals, subscriber_buffer: 1024, max_pending: 65536 }
//! ```

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgListener;
use time::OffsetDateTime;
use tokio::sync::{Notify, watch};
use tp_contracts::bus::{Event, EventBus, EventKind, EventStream};
use tp_contracts::config::duration;
use tp_contracts::meta::AccountScope;
use tp_contracts::secrets::SecretStore;
use tp_contracts::{AccountId, Error, Result, TenantId};

use crate::commits::{Histogram, Recorder, commit_metrics, observe_commit};
use crate::failover::{Failover, connect_failover};
use crate::pool::PoolConfig;
use crate::schema::db_err;
use crate::shards::{ShardDirectory, ShardId, ShardResolver};

/// Подсекция `meta.signals` (SRS 5.5). Параметры требуют перезапуска.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SignalsConfig {
    /// Канал LISTEN/NOTIFY: `[a-z_][a-z0-9_]*`, ≤ 63 байт; общий для узлов кластера.
    pub channel: String,
    /// Буфер сигналов подписчика; при переполнении — `Resync`.
    pub subscriber_buffer: usize,
    /// Сигналов в очереди публикатора; больше — отбрасываются, все подписчики получают `Resync`.
    pub max_pending: usize,
    /// Наибольшая длина сообщения NOTIFY в байтах (у PostgreSQL — меньше 8000).
    pub max_payload: usize,
    /// Ожидание первого подключения слушателя при подписке; дольше — `Unavailable`.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub subscribe_timeout: Duration,
    /// Пауза перед новой попыткой подключения слушателя, когда поиск primary не удался.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub reconnect_pause: Duration,
}

impl Default for SignalsConfig {
    fn default() -> Self {
        Self {
            channel: "tp_signals".to_owned(),
            subscriber_buffer: 1024,
            max_pending: 65536,
            max_payload: 7900,
            subscribe_timeout: Duration::from_secs(10),
            reconnect_pause: Duration::from_secs(1),
        }
    }
}

impl SignalsConfig {
    /// Проверяет подсекцию: имя канала, размеры.
    pub fn check(&self) -> Result<()> {
        let c = &self.channel;
        let valid = !c.is_empty()
            && c.len() <= 63
            && c.starts_with(|ch: char| ch.is_ascii_lowercase() || ch == '_')
            && c.chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_');
        if !valid {
            return Err(Error::invalid(
                "meta.signals.channel: нужно [a-z_][a-z0-9_]*, не длиннее 63 байт",
            ));
        }
        if self.subscriber_buffer == 0 || self.max_pending == 0 {
            return Err(Error::invalid(
                "meta.signals: subscriber_buffer и max_pending должны быть больше нуля",
            ));
        }
        if !(512..8000).contains(&self.max_payload) {
            return Err(Error::invalid(
                "meta.signals.max_payload: от 512 до 7999 байт",
            ));
        }
        if self.subscribe_timeout.is_zero() || self.reconnect_pause.is_zero() {
            return Err(Error::invalid(
                "meta.signals: subscribe_timeout и reconnect_pause должны быть больше нуля",
            ));
        }
        Ok(())
    }
}

/// Снимок метрик шины.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignalMetrics {
    /// Сигналов принято к публикации.
    pub published: u64,
    /// Одинаковых сигналов склеено в очереди публикатора.
    pub coalesced: u64,
    /// Сигналов отброшено при переполнении очереди публикатора или ошибке отправки.
    pub dropped: u64,
    /// Отправок NOTIFY (пачек).
    pub batches: u64,
    /// Сигналов в отправленных пачках.
    pub batched: u64,
    /// Наибольший размер пачки (сигналов).
    pub batch_max: u64,
    /// Ошибок отправки.
    pub publish_errors: u64,
    /// Время NOTIFY: последней отправки, суммарное, наибольшее.
    pub notify_last: Duration,
    /// Суммарное время NOTIFY.
    pub notify_total: Duration,
    /// Наибольшее время NOTIFY.
    pub notify_max: Duration,
    /// Сигналов принято слушателем.
    pub received: u64,
    /// Задержка сигнала (от постановки в очередь до приёма): последняя.
    pub latency_last: Duration,
    /// Суммарная задержка по принятым сообщениям ([`SignalMetrics::latency_count`]).
    pub latency_total: Duration,
    /// Наибольшая задержка.
    pub latency_max: Duration,
    /// Принятых сообщений NOTIFY.
    pub latency_count: u64,
    /// `Resync` подписчикам из-за переполнения их буфера.
    pub overflows: u64,
    /// Переподключений слушателя.
    pub reconnects: u64,
    /// Время фиксаций с NOTIFY (отправок публикатора; команда с неявной фиксацией) — гистограмма.
    pub notify_commit: Histogram,
    /// Время всех фиксаций процесса на primary, включая NOTIFY ([`commit_metrics`]) — гистограмма.
    pub commit: Histogram,
    /// Соединений-слушателей узла сейчас (по одному на шину с подключённым слушателем).
    pub listeners: u64,
}

impl SignalMetrics {
    /// Доля времени фиксации, занятая NOTIFY (критерий Д.7 — не больше 0,05): сумма `notify_commit` к сумме
    /// `commit`; `None` — фиксаций ещё не было.
    pub fn notify_commit_share(&self) -> Option<f64> {
        let total = self.commit.sum.as_secs_f64();
        (total > 0.0).then(|| (self.notify_commit.sum.as_secs_f64() / total).min(1.0))
    }

    /// Сумма снимков шин (шарды): счётчики и суммы складываются, `*_max` — наибольшее, `*_last` — наибольшее
    /// из последних; `commit` — общая для процесса, не складывается.
    #[must_use]
    pub fn merged(self, o: &Self) -> Self {
        Self {
            published: self.published + o.published,
            coalesced: self.coalesced + o.coalesced,
            dropped: self.dropped + o.dropped,
            batches: self.batches + o.batches,
            batched: self.batched + o.batched,
            batch_max: self.batch_max.max(o.batch_max),
            publish_errors: self.publish_errors + o.publish_errors,
            notify_last: self.notify_last.max(o.notify_last),
            notify_total: self.notify_total.saturating_add(o.notify_total),
            notify_max: self.notify_max.max(o.notify_max),
            received: self.received + o.received,
            latency_last: self.latency_last.max(o.latency_last),
            latency_total: self.latency_total.saturating_add(o.latency_total),
            latency_max: self.latency_max.max(o.latency_max),
            latency_count: self.latency_count + o.latency_count,
            overflows: self.overflows + o.overflows,
            reconnects: self.reconnects + o.reconnects,
            notify_commit: self.notify_commit.merged(&o.notify_commit),
            commit: o.commit,
            listeners: self.listeners + o.listeners,
        }
    }
}

#[derive(Debug, Default)]
struct Counters {
    published: AtomicU64,
    coalesced: AtomicU64,
    dropped: AtomicU64,
    batches: AtomicU64,
    batched: AtomicU64,
    batch_max: AtomicU64,
    publish_errors: AtomicU64,
    notify_last_us: AtomicU64,
    notify_total_us: AtomicU64,
    notify_max_us: AtomicU64,
    received: AtomicU64,
    latency_last_us: AtomicU64,
    latency_total_us: AtomicU64,
    latency_max_us: AtomicU64,
    latency_count: AtomicU64,
    overflows: AtomicU64,
    reconnects: AtomicU64,
    notify_commit: Recorder,
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn add(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Проверка сигнала перед публикацией: `Resync` вставляет только шина; системный вид — без учётной записи.
pub(crate) fn check_event(event: &Event) -> Result<()> {
    if event.kind == EventKind::Resync {
        return Err(Error::invalid(
            "Resync is inserted by the bus, not published",
        ));
    }
    if event.kind.is_system() != event.account.is_none() {
        return Err(Error::invalid(
            "system signals have no account, account signals have one",
        ));
    }
    Ok(())
}

// ---------- Подписчики ----------

#[derive(Debug)]
struct Sub {
    state: Mutex<SubState>,
    notify: Notify,
    resync: Event,
    cap: usize,
}

#[derive(Debug, Default)]
struct SubState {
    queue: VecDeque<Event>,
    closed: bool,
}

impl Sub {
    /// Сигнал в буфер; `true` — буфер переполнен, накопленное заменено на `Resync`.
    fn push(&self, e: Event) -> bool {
        let mut st = lock(&self.state);
        if st.closed {
            return false;
        }
        let overflow = st.queue.len() >= self.cap;
        if overflow {
            // Сверка с хранилищем покрывает и отброшенные сигналы, и этот.
            st.queue.clear();
            st.queue.push_back(self.resync.clone());
        } else {
            st.queue.push_back(e);
        }
        drop(st);
        self.notify.notify_one();
        overflow
    }

    fn resync(&self) {
        let mut st = lock(&self.state);
        if st.closed {
            return;
        }
        st.queue.clear();
        st.queue.push_back(self.resync.clone());
        drop(st);
        self.notify.notify_one();
    }

    fn close(&self) {
        lock(&self.state).closed = true;
        self.notify.notify_one();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Interest {
    Account(TenantId, AccountId),
    System,
}

#[derive(Debug, Default)]
struct HubState {
    next: u64,
    subs: HashMap<Interest, Vec<(u64, Arc<Sub>)>>,
    closed: bool,
}

/// Подписчики узла по интересам.
#[derive(Debug, Default)]
struct Hub {
    state: Mutex<HubState>,
}

impl Hub {
    fn add(&self, interest: Interest, sub: Arc<Sub>) -> u64 {
        let mut st = lock(&self.state);
        if st.closed {
            sub.close();
        }
        st.next += 1;
        let id = st.next;
        st.subs.entry(interest).or_default().push((id, sub));
        id
    }

    fn remove(&self, interest: Interest, id: u64) {
        let mut st = lock(&self.state);
        if let Some(v) = st.subs.get_mut(&interest) {
            v.retain(|(i, _)| *i != id);
            if v.is_empty() {
                st.subs.remove(&interest);
            }
        }
    }

    fn matching(&self, e: &Event) -> Vec<Arc<Sub>> {
        let interest = match e.account {
            Some(a) => Interest::Account(e.tenant, a),
            None => Interest::System,
        };
        lock(&self.state)
            .subs
            .get(&interest)
            .map(|v| v.iter().map(|(_, s)| Arc::clone(s)).collect())
            .unwrap_or_default()
    }

    /// Раздаёт сигнал подписчикам; возвращает число переполнений.
    fn dispatch(&self, e: &Event) -> u64 {
        let mut overflows = 0;
        for s in self.matching(e) {
            if s.push(e.clone()) {
                overflows += 1;
            }
        }
        overflows
    }

    fn all(&self) -> Vec<Arc<Sub>> {
        lock(&self.state)
            .subs
            .values()
            .flatten()
            .map(|(_, s)| Arc::clone(s))
            .collect()
    }

    fn resync_all(&self) {
        for s in self.all() {
            s.resync();
        }
    }

    fn close_all(&self) {
        lock(&self.state).closed = true;
        for s in self.all() {
            s.close();
        }
    }

    fn len(&self) -> usize {
        lock(&self.state).subs.values().map(Vec::len).sum()
    }
}

/// Подписка на сигналы шины: [`Subscription::next`] — следующий сигнал или `Resync`; `None` — шина
/// остановлена. Удаление подписки — `drop`.
#[derive(Debug)]
pub struct Subscription {
    sub: Arc<Sub>,
    hub: Arc<Hub>,
    interest: Interest,
    id: u64,
}

impl Subscription {
    /// Следующий сигнал подписки; `None` — шина остановлена (подписчик переподписывается и сверяется).
    pub async fn next(&mut self) -> Option<Event> {
        loop {
            {
                let mut st = lock(&self.sub.state);
                if let Some(e) = st.queue.pop_front() {
                    return Some(e);
                }
                if st.closed {
                    return None;
                }
            }
            self.sub.notify.notified().await;
        }
    }

    /// Подписка как поток контракта [`EventStream`]: завершается вместе с подпиской, `drop` потока удаляет её.
    pub fn into_stream(self) -> EventStream {
        Box::pin(futures::stream::unfold(self, |mut sub| async move {
            sub.next().await.map(|e| (e, sub))
        }))
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.hub.remove(self.interest, self.id);
    }
}

// ---------- Очередь публикатора ----------

#[derive(Debug, Default)]
struct Pending {
    /// Сигналы в JSON по порядку постановки.
    events: Vec<String>,
    seen: HashSet<String>,
    /// Момент постановки самого раннего сигнала очереди.
    since: Option<OffsetDateTime>,
    /// Часть сигналов отброшена: получатели сверяются.
    resync: bool,
}

/// Пачка к отправке.
#[derive(Debug, Default)]
struct Batch {
    events: Vec<String>,
    since: Option<OffsetDateTime>,
    resync: bool,
}

/// Сообщение NOTIFY.
#[derive(Debug, Deserialize)]
struct Wire {
    t: i64,
    #[serde(default)]
    r: bool,
    #[serde(default)]
    e: Vec<Event>,
}

fn unix_micros(t: OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos() / 1000).unwrap_or(i64::MAX)
}

/// Делит пачку на сообщения NOTIFY не длиннее `max` байт; признак сверки — в первом. Сигнал длиннее
/// сообщения отбрасывается (второе значение — их число), признак сверки тогда ставится.
fn payloads(batch: &Batch, max: usize) -> (Vec<String>, u64) {
    let t = unix_micros(batch.since.unwrap_or_else(OffsetDateTime::now_utc));
    let head = |r: bool| format!(r#"{{"t":{t},"r":{r},"e":["#);
    let mut resync = batch.resync;
    let mut dropped = 0;
    let fits = |e: &String| head(true).len() + e.len() + 2 <= max;
    if batch.events.iter().any(|e| !fits(e)) {
        resync = true;
    }
    let mut out = Vec::new();
    let mut cur = head(resync);
    let mut n = 0usize;
    for e in &batch.events {
        if !fits(e) {
            dropped += 1;
            continue;
        }
        if n > 0 && cur.len() + 1 + e.len() + 2 > max {
            cur.push_str("]}");
            out.push(std::mem::replace(&mut cur, head(false)));
            n = 0;
        }
        if n > 0 {
            cur.push(',');
        }
        cur.push_str(e);
        n += 1;
    }
    if n > 0 || out.is_empty() {
        cur.push_str("]}");
        out.push(cur);
    }
    (out, dropped)
}

// ---------- Шина ----------

#[derive(Debug)]
struct Shared {
    cfg: SignalsConfig,
    hub: Arc<Hub>,
    pending: Mutex<Pending>,
    wake: Notify,
    counters: Counters,
    connected: watch::Sender<bool>,
}

impl Shared {
    fn take(&self) -> Batch {
        let mut p = lock(&self.pending);
        let p = std::mem::take(&mut *p);
        Batch {
            events: p.events,
            since: p.since,
            resync: p.resync,
        }
    }

    fn note_notify(&self, took: Duration, n: u64) {
        let c = &self.counters;
        let us = micros(took);
        add(&c.batches, 1);
        add(&c.batched, n);
        c.batch_max.fetch_max(n, Ordering::Relaxed);
        c.notify_last_us.store(us, Ordering::Relaxed);
        add(&c.notify_total_us, us);
        c.notify_max_us.fetch_max(us, Ordering::Relaxed);
        c.notify_commit.observe(took);
        observe_commit(took);
    }

    /// Принятое сообщение NOTIFY: сверка (если отмечена) и раздача подписчикам.
    fn receive(&self, payload: &str) {
        let wire: Wire = match serde_json::from_str(payload) {
            Ok(w) => w,
            Err(e) => {
                // Чужое или повреждённое сообщение: сигналы могли потеряться.
                tracing::warn!(error = %e, "сигналы: сообщение NOTIFY не разобрано — сверка подписчиков");
                self.hub.resync_all();
                return;
            }
        };
        let c = &self.counters;
        let now = unix_micros(OffsetDateTime::now_utc());
        let lat = u64::try_from(now.saturating_sub(wire.t)).unwrap_or(0);
        c.latency_last_us.store(lat, Ordering::Relaxed);
        add(&c.latency_total_us, lat);
        c.latency_max_us.fetch_max(lat, Ordering::Relaxed);
        add(&c.latency_count, 1);
        if wire.r {
            self.hub.resync_all();
        }
        for e in wire.e {
            if check_event(&e).is_err() {
                continue;
            }
            add(&c.received, 1);
            let overflows = self.hub.dispatch(&e);
            add(&c.overflows, overflows);
        }
    }
}

/// Останавливает задачи шины, когда отпущен последний экземпляр [`PgSignalBus`].
#[derive(Debug)]
struct Stop {
    tx: watch::Sender<bool>,
    hub: Arc<Hub>,
}

impl Drop for Stop {
    fn drop(&mut self) {
        self.tx.send_replace(true);
        self.hub.close_all();
    }
}

/// Шина сигналов кластера на PostgreSQL LISTEN/NOTIFY. Клон — та же шина; задачи публикатора и слушателя
/// останавливаются, когда отпущен последний клон (подписки тогда завершаются).
#[derive(Debug, Clone)]
pub struct PgSignalBus {
    shared: Arc<Shared>,
    _stop: Arc<Stop>,
}

impl PgSignalBus {
    /// Запускает публикатор над пулом узла `publish` и слушатель над прямым соединением `listen`
    /// ([`connect_listener`]). Подключение слушателя — в фоне; подписка ждёт его до `subscribe_timeout`.
    pub fn start(publish: Failover, listen: Failover, cfg: SignalsConfig) -> Result<Self> {
        cfg.check()?;
        let hub = Arc::new(Hub::default());
        let shared = Arc::new(Shared {
            cfg,
            hub: Arc::clone(&hub),
            pending: Mutex::new(Pending::default()),
            wake: Notify::new(),
            counters: Counters::default(),
            connected: watch::Sender::new(false),
        });
        let (tx, stop) = watch::channel(false);
        let switches = publish.switches();
        tokio::spawn(run_publisher(Arc::clone(&shared), publish, stop.clone()));
        tokio::spawn(run_listener(Arc::clone(&shared), listen, switches, stop));
        Ok(Self {
            shared,
            _stop: Arc::new(Stop { tx, hub }),
        })
    }

    /// Ставит сигнал в очередь публикатора (изменение уже зафиксировано). `Resync` и системный вид с учётной
    /// записью (или вид учётной записи без неё) — `Invalid`.
    pub async fn publish(&self, event: Event) -> Result<()> {
        check_event(&event)?;
        let json = serde_json::to_string(&event).map_err(|e| Error::internal(e.to_string()))?;
        let s = &self.shared;
        add(&s.counters.published, 1);
        {
            let mut p = lock(&s.pending);
            if p.seen.contains(&json) {
                add(&s.counters.coalesced, 1);
                return Ok(());
            }
            if p.events.len() >= s.cfg.max_pending {
                let n = u64::try_from(p.events.len()).unwrap_or(u64::MAX);
                add(&s.counters.dropped, n);
                if !p.resync {
                    tracing::warn!(
                        "сигналы: очередь публикатора переполнена (meta.signals.max_pending) — сверка подписчиков"
                    );
                }
                p.events.clear();
                p.seen.clear();
                p.resync = true;
            }
            p.since.get_or_insert_with(OffsetDateTime::now_utc);
            p.seen.insert(json.clone());
            p.events.push(json);
        }
        s.wake.notify_one();
        Ok(())
    }

    /// Сигналы учётной записи `scope` и `Resync` подписки.
    pub async fn subscribe(&self, scope: AccountScope) -> Result<Subscription> {
        let resync = Event {
            tenant: scope.tenant,
            account: Some(scope.account),
            kind: EventKind::Resync,
        };
        self.add(Interest::Account(scope.tenant, scope.account), resync)
            .await
    }

    /// Системные сигналы всех организаций и `Resync` подписки.
    pub async fn subscribe_system(&self) -> Result<Subscription> {
        let resync = Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind: EventKind::Resync,
        };
        self.add(Interest::System, resync).await
    }

    async fn add(&self, interest: Interest, resync: Event) -> Result<Subscription> {
        let s = &self.shared;
        // Сигналы, опубликованные после подписки, не теряются: слушатель уже подключён. Подписчики,
        // добавленные во время переподключения, получат `Resync` после него.
        let mut connected = s.connected.subscribe();
        let wait = connected.wait_for(|c| *c);
        match tokio::time::timeout(s.cfg.subscribe_timeout, wait).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => {
                return Err(Error::Unavailable {
                    retry_after: Some(s.cfg.reconnect_pause),
                });
            }
        }
        let sub = Arc::new(Sub {
            state: Mutex::new(SubState::default()),
            notify: Notify::new(),
            resync,
            cap: s.cfg.subscriber_buffer,
        });
        let id = s.hub.add(interest, Arc::clone(&sub));
        Ok(Subscription {
            sub,
            hub: Arc::clone(&s.hub),
            interest,
            id,
        })
    }

    /// Слушатель подключён.
    pub fn connected(&self) -> bool {
        *self.shared.connected.borrow()
    }

    /// Число подписок узла.
    pub fn subscribers(&self) -> usize {
        self.shared.hub.len()
    }

    /// Снимок метрик.
    pub fn metrics(&self) -> SignalMetrics {
        let c = &self.shared.counters;
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let d = |a: &AtomicU64| Duration::from_micros(a.load(Ordering::Relaxed));
        SignalMetrics {
            published: g(&c.published),
            coalesced: g(&c.coalesced),
            dropped: g(&c.dropped),
            batches: g(&c.batches),
            batched: g(&c.batched),
            batch_max: g(&c.batch_max),
            publish_errors: g(&c.publish_errors),
            notify_last: d(&c.notify_last_us),
            notify_total: d(&c.notify_total_us),
            notify_max: d(&c.notify_max_us),
            received: g(&c.received),
            latency_last: d(&c.latency_last_us),
            latency_total: d(&c.latency_total_us),
            latency_max: d(&c.latency_max_us),
            latency_count: g(&c.latency_count),
            overflows: g(&c.overflows),
            reconnects: g(&c.reconnects),
            notify_commit: c.notify_commit.snapshot(),
            commit: commit_metrics(),
            listeners: u64::from(self.connected()),
        }
    }
}

#[async_trait]
impl EventBus for PgSignalBus {
    async fn publish(&self, event: Event) -> Result<()> {
        PgSignalBus::publish(self, event).await
    }

    async fn subscribe(&self, scope: AccountScope) -> Result<EventStream> {
        Ok(PgSignalBus::subscribe(self, scope).await?.into_stream())
    }

    async fn subscribe_system(&self) -> Result<EventStream> {
        Ok(PgSignalBus::subscribe_system(self).await?.into_stream())
    }
}

/// Сигналы кластера по шардам справочника: шина [`PgSignalBus`] на primary каждого шарда (см. описание
/// модуля). Шарды, добавленные в справочник после запуска, подключает [`ShardedSignalBus::refresh`]; до этого
/// сигналы их учётных записей идут через основную БД.
#[derive(Clone)]
pub struct ShardedSignalBus {
    dir: Arc<ShardDirectory>,
    secrets: Arc<dyn SecretStore>,
    cfg: SignalsConfig,
    buses: Arc<Mutex<std::collections::BTreeMap<ShardId, PgSignalBus>>>,
}

impl std::fmt::Debug for ShardedSignalBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardedSignalBus")
            .field(
                "shards",
                &lock(&self.buses).keys().copied().collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl ShardedSignalBus {
    /// Шина основной БД `main` и шины остальных шардов справочника `dir` (учётные данные — из `secrets`).
    pub async fn start(
        main: PgSignalBus,
        dir: Arc<ShardDirectory>,
        secrets: Arc<dyn SecretStore>,
        cfg: SignalsConfig,
    ) -> Result<Self> {
        let mut buses = std::collections::BTreeMap::new();
        buses.insert(ShardId::MAIN, main);
        let bus = Self {
            dir,
            secrets,
            cfg,
            buses: Arc::new(Mutex::new(buses)),
        };
        bus.refresh().await?;
        Ok(bus)
    }

    /// Подключает шины шардов справочника, которых ещё нет (после сигнала изменения справочника);
    /// возвращает число подключённых. Шины удалённых из справочника шардов остаются до остановки.
    pub async fn refresh(&self) -> Result<usize> {
        let mut added = 0;
        for shard in self.dir.shards().await? {
            if lock(&self.buses).contains_key(&shard) {
                continue;
            }
            let cfg = self.dir.shard_pool_config(shard).await?;
            let publish = connect_failover(&cfg, Arc::clone(&self.secrets)).await?;
            let listen = connect_listener(&cfg, Arc::clone(&self.secrets)).await?;
            let bus = PgSignalBus::start(publish, listen, self.cfg.clone())?;
            lock(&self.buses).entry(shard).or_insert(bus);
            added += 1;
        }
        Ok(added)
    }

    /// Шарды с подключённой шиной по возрастанию.
    pub fn shards(&self) -> Vec<ShardId> {
        lock(&self.buses).keys().copied().collect()
    }

    /// Снимок метрик по шинам всех шардов ([`SignalMetrics::merged`]); `listeners` — соединений-слушателей узла.
    pub fn metrics(&self) -> SignalMetrics {
        self.all()
            .iter()
            .fold(SignalMetrics::default(), |m, b| m.merged(&b.metrics()))
    }

    /// Шина шарда; не подключённого — основной БД.
    fn bus(&self, shard: ShardId) -> PgSignalBus {
        let buses = lock(&self.buses);
        match buses.get(&shard) {
            Some(b) => b.clone(),
            None => buses[&ShardId::MAIN].clone(),
        }
    }

    fn all(&self) -> Vec<PgSignalBus> {
        lock(&self.buses).values().cloned().collect()
    }
}

#[async_trait]
impl EventBus for ShardedSignalBus {
    async fn publish(&self, event: Event) -> Result<()> {
        let shard = match event.account {
            Some(account) if self.dir.shards().await?.len() > 1 => {
                let scope = AccountScope {
                    tenant: event.tenant,
                    account,
                };
                match self.dir.shard_of(scope).await {
                    Ok(s) => s,
                    // Учётной записи нет в каталоге — сигнал никому не нужен, но не теряется.
                    Err(Error::NotFound) => ShardId::MAIN,
                    Err(e) => return Err(e),
                }
            }
            _ => ShardId::MAIN,
        };
        self.bus(shard).publish(event).await
    }

    /// Шина шарда учётной записи и основной БД (сигналы до подключения шины шарда).
    async fn subscribe(&self, scope: AccountScope) -> Result<EventStream> {
        let mut streams = vec![
            self.bus(ShardId::MAIN)
                .subscribe(scope)
                .await?
                .into_stream(),
        ];
        if self.dir.shards().await?.len() > 1 {
            let shard = self.dir.shard_of(scope).await?;
            if shard != ShardId::MAIN && lock(&self.buses).contains_key(&shard) {
                streams.push(self.bus(shard).subscribe(scope).await?.into_stream());
            }
        }
        Ok(Box::pin(futures::stream::select_all(streams)))
    }

    async fn subscribe_system(&self) -> Result<EventStream> {
        let mut streams = Vec::new();
        for bus in self.all() {
            streams.push(bus.subscribe_system().await?.into_stream());
        }
        Ok(Box::pin(futures::stream::select_all(streams)))
    }
}

/// Задача публикатора: забирает накопленное и отправляет одной короткой транзакцией.
async fn run_publisher(shared: Arc<Shared>, publish: Failover, mut stop: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            () = shared.wake.notified() => {}
            r = stop.changed() => if r.is_err() || *stop.borrow() { return },
        }
        loop {
            let batch = shared.take();
            if batch.events.is_empty() && !batch.resync {
                break;
            }
            let n = u64::try_from(batch.events.len()).unwrap_or(u64::MAX);
            let (messages, too_long) = payloads(&batch, shared.cfg.max_payload);
            if too_long > 0 {
                tracing::warn!(
                    count = too_long,
                    "сигналы: сигнал длиннее meta.signals.max_payload отброшен"
                );
                add(&shared.counters.dropped, too_long);
            }
            let channel = shared.cfg.channel.clone();
            let started = Instant::now();
            let sent = publish
                .retry(|pool| {
                    let channel = channel.clone();
                    let messages = messages.clone();
                    async move {
                        sqlx::query("SELECT pg_notify($1, m) FROM unnest($2::text[]) AS m")
                            .bind(channel)
                            .bind(messages)
                            .execute(&pool)
                            .await
                            .map(drop)
                            .map_err(db_err)
                    }
                })
                .await;
            match sent {
                Ok(()) => shared.note_notify(started.elapsed(), n),
                Err(e) => {
                    // At-most-once: сигналы пачки потеряны; при переключении СУБД слушатели переподключатся
                    // и сверятся, иначе получатели сверятся при следующем запуске или Resync.
                    add(&shared.counters.publish_errors, 1);
                    add(&shared.counters.dropped, n);
                    tracing::warn!(error = %e, signals = n, "сигналы: NOTIFY не отправлен");
                }
            }
        }
    }
}

/// Исход сеанса слушателя.
enum Session {
    /// Соединение потеряно или primary пула сменился: переподключение.
    Lost,
    /// Шина остановлена.
    Stopped,
}

/// Задача слушателя: подключение к primary, LISTEN, приём; после переподключения — `Resync` всем.
async fn run_listener(
    shared: Arc<Shared>,
    listen: Failover,
    mut switches: watch::Receiver<u64>,
    mut stop: watch::Receiver<bool>,
) {
    let mut first = true;
    loop {
        if *stop.borrow() {
            return;
        }
        let channel = shared.cfg.channel.clone();
        let connect = listen.retry(|pool| {
            let channel = channel.clone();
            async move {
                let mut l = PgListener::connect_with(&pool).await.map_err(db_err)?;
                l.eager_reconnect(false);
                l.ignore_pool_close_event(true);
                l.listen(&channel).await.map_err(db_err)?;
                Ok(l)
            }
        });
        let mut listener = tokio::select! {
            r = connect => match r {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(error = %e, "сигналы: слушатель не подключён — повтор");
                    tokio::select! {
                        () = tokio::time::sleep(shared.cfg.reconnect_pause) => {}
                        _ = stop.changed() => {}
                    }
                    continue;
                }
            },
            _ = stop.changed() => return,
        };
        switches.borrow_and_update();
        shared.connected.send_replace(true);
        if first {
            first = false;
            tracing::debug!("сигналы: слушатель подключён");
        } else {
            add(&shared.counters.reconnects, 1);
            tracing::info!("сигналы: слушатель переподключён — сверка подписчиков");
            shared.hub.resync_all();
        }
        let outcome = loop {
            tokio::select! {
                r = listener.try_recv() => match r {
                    Ok(Some(n)) => shared.receive(n.payload()),
                    Ok(None) => break Session::Lost,
                    Err(e) => {
                        tracing::warn!(error = %e, "сигналы: соединение слушателя потеряно");
                        break Session::Lost;
                    }
                },
                r = switches.changed() => {
                    if r.is_ok() {
                        tracing::info!("сигналы: primary пула узла сменился — переподключение слушателя");
                    }
                    break Session::Lost;
                }
                r = stop.changed() => if r.is_err() || *stop.borrow() { break Session::Stopped },
            }
        };
        shared.connected.send_replace(false);
        drop(listener);
        if matches!(outcome, Session::Stopped) {
            return;
        }
    }
}

/// Пул прямого соединения слушателя сигналов с учётными данными узла: мимо PgBouncer (с [`PoolConfig::pgbouncer`]
/// — поиск primary по прямым адресам [`PoolConfig::direct_url`] + [`PoolConfig::direct_urls`], хотя бы один
/// обязателен), без PgBouncer — поиск primary по `url` + `urls`.
pub async fn connect_listener(cfg: &PoolConfig, secrets: Arc<dyn SecretStore>) -> Result<Failover> {
    let mut c = cfg.clone();
    if cfg.pgbouncer {
        let Some(mut direct) = cfg.direct_addresses() else {
            return Err(Error::invalid(
                "database.direct_url или direct_urls обязательны с pgbouncer: слушатель сигналов подключается мимо PgBouncer",
            ));
        };
        c.url = direct.remove(0);
        c.urls = direct;
        c.pgbouncer = false;
    }
    // Соединение слушателя (занято, сроки пула к нему не применяются) и проверка primary при переподключении;
    // простаивающее закрывается по `idle_timeout`.
    c.max_connections = 2;
    c.min_connections = 0;
    c.application_name = format!("{} listener", cfg.application_name);
    connect_failover(&c, secrets).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_contracts::events::Partition;

    fn sys(kind: EventKind) -> Event {
        Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind,
        }
    }

    #[test]
    fn config_check() {
        assert_eq!(SignalsConfig::default().check(), Ok(()));
        for bad in ["", "Tp", "1tp", "tp-signals", &"a".repeat(64)] {
            let c = SignalsConfig {
                channel: bad.to_owned(),
                ..SignalsConfig::default()
            };
            assert!(c.check().is_err(), "{bad:?}");
        }
        let c = SignalsConfig {
            max_payload: 8000,
            ..SignalsConfig::default()
        };
        assert!(c.check().is_err());
        let c: SignalsConfig =
            serde_json::from_str(r#"{"subscriber_buffer": 8, "subscribe_timeout": "2s"}"#)
                .expect("json");
        assert_eq!(c.subscriber_buffer, 8);
        assert_eq!(c.subscribe_timeout, Duration::from_secs(2));
        assert!(serde_json::from_str::<SignalsConfig>(r#"{"buffer": 8}"#).is_err());
    }

    #[test]
    fn events_checked() {
        assert!(check_event(&sys(EventKind::MembersChanged)).is_ok());
        assert!(check_event(&sys(EventKind::Resync)).is_err());
        assert!(check_event(&sys(EventKind::MailboxListChanged)).is_err());
        let with_account = Event {
            account: Some(AccountId::new()),
            ..sys(EventKind::QueueReady)
        };
        assert!(check_event(&with_account).is_err());
    }

    #[test]
    fn payloads_split_and_parse() {
        let events: Vec<String> = (0..200)
            .map(|i| {
                serde_json::to_string(&sys(EventKind::LogAppended {
                    partition: Partition(i),
                }))
                .expect("json")
            })
            .collect();
        let batch = Batch {
            events: events.clone(),
            since: Some(OffsetDateTime::now_utc()),
            resync: true,
        };
        let (msgs, dropped) = payloads(&batch, 1000);
        assert_eq!(dropped, 0);
        assert!(msgs.len() > 1);
        let mut got = Vec::new();
        for (i, m) in msgs.iter().enumerate() {
            assert!(m.len() <= 1000, "{}", m.len());
            let w: Wire = serde_json::from_str(m).expect("wire");
            assert_eq!(w.r, i == 0, "сверка — в первом сообщении");
            got.extend(w.e);
        }
        let want: Vec<Event> = events
            .iter()
            .map(|e| serde_json::from_str(e).expect("event"))
            .collect();
        assert_eq!(got, want);

        // Только признак сверки — одно сообщение без сигналов.
        let (msgs, _) = payloads(
            &Batch {
                resync: true,
                ..Batch::default()
            },
            1000,
        );
        assert_eq!(msgs.len(), 1);
        let w: Wire = serde_json::from_str(&msgs[0]).expect("wire");
        assert!(w.r && w.e.is_empty());

        // Слишком длинный сигнал отброшен, получатели сверяются.
        let (msgs, dropped) = payloads(
            &Batch {
                events: vec![format!("\"{}\"", "x".repeat(2000)), events[0].clone()],
                since: None,
                resync: false,
            },
            1000,
        );
        assert_eq!(dropped, 1);
        let w: Wire = serde_json::from_str(&msgs[0]).expect("wire");
        assert!(w.r);
        assert_eq!(w.e.len(), 1);
    }

    #[tokio::test]
    async fn hub_filters_and_overflows() {
        let hub = Arc::new(Hub::default());
        let scope = AccountScope {
            tenant: TenantId::new(),
            account: AccountId::new(),
        };
        let mk = |interest, resync: Event, cap| {
            let sub = Arc::new(Sub {
                state: Mutex::new(SubState::default()),
                notify: Notify::new(),
                resync,
                cap,
            });
            let id = hub.add(interest, Arc::clone(&sub));
            Subscription {
                sub,
                hub: Arc::clone(&hub),
                interest,
                id,
            }
        };
        let acc_resync = Event {
            tenant: scope.tenant,
            account: Some(scope.account),
            kind: EventKind::Resync,
        };
        let mut acc = mk(
            Interest::Account(scope.tenant, scope.account),
            acc_resync.clone(),
            2,
        );
        let mut system = mk(Interest::System, sys(EventKind::Resync), 2);
        let mine = Event {
            tenant: scope.tenant,
            account: Some(scope.account),
            kind: EventKind::MailboxListChanged,
        };
        let other = Event {
            account: Some(AccountId::new()),
            ..mine.clone()
        };
        assert_eq!(hub.dispatch(&other), 0);
        assert_eq!(hub.dispatch(&mine), 0);
        assert_eq!(hub.dispatch(&sys(EventKind::MembersChanged)), 0);
        assert_eq!(acc.next().await, Some(mine.clone()));
        assert_eq!(system.next().await, Some(sys(EventKind::MembersChanged)));

        // Переполнение: накопленное заменяется одним Resync, дальше — новые сигналы.
        for _ in 0..3 {
            hub.dispatch(&mine);
        }
        hub.dispatch(&mine);
        assert_eq!(acc.next().await, Some(acc_resync.clone()));
        assert_eq!(acc.next().await, Some(mine.clone()));

        hub.resync_all();
        assert_eq!(acc.next().await, Some(acc_resync));
        assert_eq!(system.next().await, Some(sys(EventKind::Resync)));

        assert_eq!(hub.len(), 2);
        drop(acc);
        assert_eq!(hub.len(), 1);
        hub.close_all();
        assert_eq!(system.next().await, None);
    }
}
