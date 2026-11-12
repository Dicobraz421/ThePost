//! Метрики пула СУБД и слушателя сигналов LISTEN (решение 57, PLT-25; отчёт по критериям Д.7).
//!
//! Платформа не зависит от драйвера СУБД: владелец пула (`tp-meta`) передаёт снимок занятости замыканием
//! (как [`Metrics::register_bus`]) и оборачивает получение соединения в [`PoolMeter::acquire`] — так считаются
//! ожидающие и время ожидания. Имена: `tp_db_pool_in_use`, `tp_db_pool_idle`, `tp_db_pool_max`,
//! `tp_db_pool_waiting`, `tp_db_pool_wait_seconds` (гистограмма), `tp_db_pool_acquire_errors_total` — метка
//! `pool`; `tp_db_listener_connected` (0/1), `tp_db_listener_subscribers`, `tp_db_listener_lag_seconds`,
//! `tp_db_listener_reconnects_total`, `tp_db_listener_received_total` — метка `listener`.

use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeMetric};
use prometheus_client::metrics::counter::{ConstCounter, Counter};
use prometheus_client::metrics::family::{Family, MetricConstructor};
use prometheus_client::metrics::gauge::{ConstGauge, Gauge};
use prometheus_client::metrics::histogram::Histogram;

use crate::telemetry::{Metrics, TelemetryConfig};

/// Имя соединения слушателя сигналов в `pg_stat_activity`: `<application_name пула> listener`.
pub fn listener_application_name(pool_application_name: &str) -> String {
    format!("{pool_application_name} listener")
}

/// Снимок занятости пула соединений.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolSample {
    /// Открыто соединений (занятые и свободные).
    pub size: u32,
    /// Свободных соединений.
    pub idle: u32,
    /// Предел размера пула.
    pub max: u32,
}

/// Снимок состояния слушателя LISTEN.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListenerSample {
    /// Соединение слушателя установлено.
    pub connected: bool,
    /// Подписчиков узла.
    pub subscribers: u64,
    /// Переподключений слушателя.
    pub reconnects: u64,
    /// Принято сообщений NOTIFY.
    pub received: u64,
    /// Задержка последнего сигнала (от постановки в очередь до приёма).
    pub lag: Duration,
}

type Labels = Vec<(&'static str, String)>;
type PoolSource = Box<dyn Fn() -> PoolSample + Send + Sync>;
type ListenerSource = Box<dyn Fn() -> ListenerSample + Send + Sync>;

#[derive(Clone)]
struct Buckets(Arc<[f64]>);

impl MetricConstructor<Histogram> for Buckets {
    fn new_metric(&self) -> Histogram {
        Histogram::new(self.0.iter().copied())
    }
}

#[derive(Default)]
struct Sources {
    pools: Vec<(Labels, PoolSource)>,
    listeners: Vec<(Labels, ListenerSource)>,
}

/// Метрики СУБД узла: пулы и слушатели добавляются по имени. Клонируется.
#[derive(Clone)]
pub struct DbMetrics {
    waiting: Family<Labels, Gauge>,
    wait: Family<Labels, Histogram, Buckets>,
    errors: Family<Labels, Counter>,
    sources: Arc<RwLock<Sources>>,
}

impl fmt::Debug for DbMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DbMetrics")
    }
}

impl DbMetrics {
    /// Регистрирует метрики в реестре узла; границы гистограммы ожидания — `telemetry.db_wait_buckets_ms`.
    pub fn register(metrics: &Metrics, cfg: &TelemetryConfig) -> Self {
        let buckets = cfg
            .db_wait_buckets_ms
            .iter()
            .map(|ms| Duration::from_millis(*ms).as_secs_f64())
            .collect();
        let this = Self {
            waiting: Family::default(),
            wait: Family::new_with_constructor(Buckets(buckets)),
            errors: Family::default(),
            sources: Arc::default(),
        };
        metrics.register(
            "tp_db_pool_waiting",
            "Tasks waiting for a database connection",
            this.waiting.clone(),
        );
        metrics.register(
            "tp_db_pool_wait_seconds",
            "Time spent waiting for a database connection",
            this.wait.clone(),
        );
        metrics.register(
            "tp_db_pool_acquire_errors",
            "Failed attempts to get a database connection (timeouts included)",
            this.errors.clone(),
        );
        metrics.register_collector(Box::new(DbCollector(this.sources.clone())));
        this
    }

    /// Добавляет пул `name` (`primary`, `replica`, шард): `snapshot` вызывается на каждый `/metrics`.
    pub fn pool(
        &self,
        name: &str,
        snapshot: impl Fn() -> PoolSample + Send + Sync + 'static,
    ) -> PoolMeter {
        let labels = vec![("pool", name.to_owned())];
        self.sources
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .pools
            .push((labels.clone(), Box::new(snapshot)));
        PoolMeter {
            waiting: self.waiting.get_or_create(&labels).clone(),
            wait: self.wait.get_or_create(&labels).clone(),
            errors: self.errors.get_or_create(&labels).clone(),
        }
    }

    /// Добавляет слушатель LISTEN `name`: `snapshot` вызывается на каждый `/metrics`.
    pub fn listener(
        &self,
        name: &str,
        snapshot: impl Fn() -> ListenerSample + Send + Sync + 'static,
    ) {
        self.sources
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .listeners
            .push((vec![("listener", name.to_owned())], Box::new(snapshot)));
    }
}

/// Счётчики ожидания одного пула. Клонируется.
#[derive(Debug, Clone)]
pub struct PoolMeter {
    waiting: Gauge,
    wait: Histogram,
    errors: Counter,
}

impl PoolMeter {
    /// Оборачивает получение соединения (`pool.acquire()`, `pool.begin()`): на время ожидания задача учтена в
    /// `tp_db_pool_waiting`, длительность — в `tp_db_pool_wait_seconds`, отказ — в `…_acquire_errors_total`.
    /// Отмена ожидания (drop) счётчик ожидающих возвращает.
    pub async fn acquire<T, E>(&self, acquire: impl Future<Output = Result<T, E>>) -> Result<T, E> {
        struct Waiting<'a>(&'a PoolMeter, Instant);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.waiting.dec();
                self.0.wait.observe(self.1.elapsed().as_secs_f64());
            }
        }
        self.waiting.inc();
        let guard = Waiting(self, Instant::now());
        let out = acquire.await;
        drop(guard);
        if out.is_err() {
            self.errors.inc();
        }
        out
    }
}

/// Снимает занятость пулов и состояние слушателей при каждом кодировании реестра.
struct DbCollector(Arc<RwLock<Sources>>);

impl fmt::Debug for DbCollector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DbCollector")
    }
}

fn gauges<N>(
    encoder: &mut DescriptorEncoder<'_>,
    name: &str,
    help: &str,
    values: &[(&Labels, N)],
) -> fmt::Result
where
    N: Copy,
    ConstGauge<N>: EncodeMetric,
{
    let Some((_, first)) = values.first() else {
        return Ok(());
    };
    let mut family =
        encoder.encode_descriptor(name, help, None, ConstGauge::new(*first).metric_type())?;
    for (labels, value) in values {
        ConstGauge::new(*value).encode(family.encode_family(*labels)?)?;
    }
    Ok(())
}

fn counters(
    encoder: &mut DescriptorEncoder<'_>,
    name: &str,
    help: &str,
    values: &[(&Labels, u64)],
) -> fmt::Result {
    let Some((_, first)) = values.first() else {
        return Ok(());
    };
    let mut family =
        encoder.encode_descriptor(name, help, None, ConstCounter::new(*first).metric_type())?;
    for (labels, value) in values {
        ConstCounter::new(*value).encode(family.encode_family(*labels)?)?;
    }
    Ok(())
}

fn as_gauge(values: Vec<(&Labels, u64)>) -> Vec<(&Labels, i64)> {
    values
        .into_iter()
        .map(|(l, n)| (l, i64::try_from(n).unwrap_or(i64::MAX)))
        .collect()
}

impl Collector for DbCollector {
    fn encode(&self, mut encoder: DescriptorEncoder) -> fmt::Result {
        let sources = self.0.read().unwrap_or_else(PoisonError::into_inner);
        let pools: Vec<_> = sources.pools.iter().map(|(l, f)| (l, f())).collect();
        let listeners: Vec<_> = sources.listeners.iter().map(|(l, f)| (l, f())).collect();
        let e = &mut encoder;

        let pool = |f: fn(&PoolSample) -> u32| -> Vec<(&Labels, i64)> {
            pools.iter().map(|(l, s)| (*l, i64::from(f(s)))).collect()
        };
        gauges(
            e,
            "tp_db_pool_in_use",
            "Database connections in use",
            &pool(|s| s.size.saturating_sub(s.idle)),
        )?;
        gauges(
            e,
            "tp_db_pool_idle",
            "Idle database connections",
            &pool(|s| s.idle),
        )?;
        gauges(
            e,
            "tp_db_pool_max",
            "Database pool size limit",
            &pool(|s| s.max),
        )?;

        let listener = |f: fn(&ListenerSample) -> u64| -> Vec<(&Labels, u64)> {
            listeners.iter().map(|(l, s)| (*l, f(s))).collect()
        };
        gauges(
            e,
            "tp_db_listener_connected",
            "LISTEN connection is up (1) or down (0)",
            &as_gauge(listener(|s| u64::from(s.connected))),
        )?;
        gauges(
            e,
            "tp_db_listener_subscribers",
            "Local subscribers of the signal listener",
            &as_gauge(listener(|s| s.subscribers)),
        )?;
        let lag: Vec<(&Labels, f64)> = listeners
            .iter()
            .map(|(l, s)| (*l, s.lag.as_secs_f64()))
            .collect();
        gauges(
            e,
            "tp_db_listener_lag_seconds",
            "Delay of the last signal from enqueue to receipt",
            &lag,
        )?;
        counters(
            e,
            "tp_db_listener_reconnects",
            "Signal listener reconnects",
            &listener(|s| s.reconnects),
        )?;
        counters(
            e,
            "tp_db_listener_received",
            "NOTIFY messages received by the listener",
            &listener(|s| s.received),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use tokio::sync::Semaphore;

    use super::*;

    /// Значение серии `name{…}` из текста метрик.
    fn value(text: &str, series: &str) -> f64 {
        text.lines()
            .find_map(|l| l.strip_prefix(series))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("{series}: {text}"))
    }

    /// Приёмка PLT-25: при исчерпании пула растёт время ожидания.
    #[tokio::test]
    async fn wait_time_grows_when_pool_is_exhausted() {
        let metrics = Metrics::default();
        let db = DbMetrics::register(&metrics, &TelemetryConfig::default());
        let slots = Arc::new(Semaphore::new(1));
        let busy = Arc::new(AtomicU32::new(0));
        let probe = busy.clone();
        let meter = db.pool("primary", move || PoolSample {
            size: 1,
            idle: 1 - probe.load(Ordering::Relaxed),
            max: 1,
        });

        // Свободный пул: соединение сразу, ожидания нет.
        let held = meter
            .acquire(slots.clone().acquire_owned())
            .await
            .expect("acquire");
        busy.store(1, Ordering::Relaxed);
        let text = metrics.encode().expect("encode");
        let free = value(&text, "tp_db_pool_wait_seconds_sum{pool=\"primary\"}");
        assert!(free < 0.001, "{text}");
        assert_eq!(value(&text, "tp_db_pool_in_use{pool=\"primary\"}"), 1.0);
        assert_eq!(value(&text, "tp_db_pool_idle{pool=\"primary\"}"), 0.0);
        assert_eq!(value(&text, "tp_db_pool_max{pool=\"primary\"}"), 1.0);

        // Пул исчерпан: вторая задача ждёт, пока соединение не вернут.
        let waiter = tokio::spawn({
            let (meter, slots) = (meter.clone(), slots.clone());
            async move { meter.acquire(slots.acquire_owned()).await.map(drop) }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let text = metrics.encode().expect("encode");
        assert_eq!(value(&text, "tp_db_pool_waiting{pool=\"primary\"}"), 1.0);
        drop(held);
        waiter.await.expect("join").expect("acquire");

        let text = metrics.encode().expect("encode");
        let exhausted = value(&text, "tp_db_pool_wait_seconds_sum{pool=\"primary\"}");
        assert!(exhausted >= free + 0.3, "{text}");
        assert_eq!(value(&text, "tp_db_pool_waiting{pool=\"primary\"}"), 0.0);
        assert_eq!(
            value(&text, "tp_db_pool_wait_seconds_count{pool=\"primary\"}"),
            2.0
        );
        // Ожидание от 300 мс не попадает в корзину «до 250 мс».
        assert_eq!(
            value(
                &text,
                "tp_db_pool_wait_seconds_bucket{le=\"0.25\",pool=\"primary\"}"
            ),
            1.0
        );
    }

    #[tokio::test]
    async fn timeout_and_cancel_are_counted() {
        let metrics = Metrics::default();
        let db = DbMetrics::register(&metrics, &TelemetryConfig::default());
        let meter = db.pool("replica", PoolSample::default);
        let slots = Arc::new(Semaphore::new(0));

        let timed_out = meter
            .acquire(tokio::time::timeout(
                Duration::from_millis(200),
                slots.clone().acquire_owned(),
            ))
            .await;
        assert!(timed_out.is_err());
        // Отмена ожидания: задача не остаётся в «ожидающих».
        let cancelled = tokio::time::timeout(
            Duration::from_millis(100),
            meter.acquire(slots.clone().acquire_owned()),
        )
        .await;
        assert!(cancelled.is_err());

        let text = metrics.encode().expect("encode");
        assert_eq!(
            value(&text, "tp_db_pool_acquire_errors_total{pool=\"replica\"}"),
            1.0
        );
        assert_eq!(value(&text, "tp_db_pool_waiting{pool=\"replica\"}"), 0.0);
        let sum = value(&text, "tp_db_pool_wait_seconds_sum{pool=\"replica\"}");
        assert!(sum >= 0.3, "{text}");
    }

    #[test]
    fn listener_state_and_application_name() {
        let metrics = Metrics::default();
        let db = DbMetrics::register(&metrics, &TelemetryConfig::default());
        let text = metrics.encode().expect("encode");
        assert!(!text.contains("tp_db_listener_connected"), "{text}");

        let sample = Arc::new(RwLock::new(ListenerSample::default()));
        let source = sample.clone();
        db.listener("signals", move || {
            *source.read().unwrap_or_else(PoisonError::into_inner)
        });
        let text = metrics.encode().expect("encode");
        assert!(
            text.contains("tp_db_listener_connected{listener=\"signals\"} 0"),
            "{text}"
        );

        *sample.write().unwrap_or_else(PoisonError::into_inner) = ListenerSample {
            connected: true,
            subscribers: 7,
            reconnects: 2,
            received: 40,
            lag: Duration::from_millis(250),
        };
        let text = metrics.encode().expect("encode");
        for line in [
            "tp_db_listener_connected{listener=\"signals\"} 1",
            "tp_db_listener_subscribers{listener=\"signals\"} 7",
            "tp_db_listener_lag_seconds{listener=\"signals\"} 0.25",
            "tp_db_listener_reconnects_total{listener=\"signals\"} 2",
            "tp_db_listener_received_total{listener=\"signals\"} 40",
            "# TYPE tp_db_listener_reconnects counter",
        ] {
            assert!(text.contains(line), "{line}: {text}");
        }
        assert_eq!(listener_application_name("thepost"), "thepost listener");
    }
}
