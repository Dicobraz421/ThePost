//! Метрики сервисов приложения, которые не регистрируют сами крейты (APP-04): счётчики кэша индексов поиска
//! и индексатора, переключение СУБД (APP-14). Значения читаются на каждый запрос `/metrics` — без своих таймеров.
//!
//! Метрики критериев Д.7 (APP-34): пул соединений узла и слушатель LISTEN (`tp_platform::DbMetrics`), время
//! фиксаций с NOTIFY и число слушателей ([`register_db`]), «сироты» BLOB ([`register_orphans`]).

use std::fmt;
use std::sync::Arc;

use prometheus_client::encoding::{EncodeMetric, MetricEncoder, NoLabelSet};
use prometheus_client::metrics::MetricType;
use tp_blob::OrphanMetrics;
use tp_contracts::acquire::AcquireObserver;
use tp_meta::commits::{HISTOGRAM_BUCKETS, Histogram};
use tp_meta::failover::{Failover, FailoverMetrics};
use tp_meta::signals::{PgSignalBus, SignalMetrics};
use tp_platform::{DbMetrics, ListenerSample, Metrics, PoolSample};
use tp_search::{CacheStats, IndexCache, Indexer, IndexerStats};

use crate::roles::OrphanProbe;

/// Описание показателя: имя, пояснение, значение `V` из снимка `S`, счётчик (иначе — текущее значение).
type StatDef<S, V = u64> = (&'static str, &'static str, fn(&S) -> V, bool);

/// Значение показателя: целое или секунды.
trait StatValue: 'static {
    fn encode(&self, encoder: MetricEncoder, counter: bool) -> Result<(), fmt::Error>;
}

impl StatValue for u64 {
    fn encode(&self, mut encoder: MetricEncoder, counter: bool) -> Result<(), fmt::Error> {
        if counter {
            encoder.encode_counter::<NoLabelSet, _, u64>(self, None)
        } else {
            encoder.encode_gauge(&i64::try_from(*self).unwrap_or(i64::MAX))
        }
    }
}

impl StatValue for f64 {
    fn encode(&self, mut encoder: MetricEncoder, counter: bool) -> Result<(), fmt::Error> {
        if counter {
            encoder.encode_counter::<NoLabelSet, _, f64>(self, None)
        } else {
            encoder.encode_gauge(self)
        }
    }
}

/// Показатель источника `T` со снимком `S`: счётчик или текущее значение.
struct Stat<T, S, V> {
    source: Arc<T>,
    snapshot: fn(&T) -> S,
    pick: fn(&S) -> V,
    counter: bool,
}

impl<T, S, V> fmt::Debug for Stat<T, S, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stat")
            .field("counter", &self.counter)
            .finish_non_exhaustive()
    }
}

impl<T: Send + Sync, S, V: StatValue> EncodeMetric for Stat<T, S, V> {
    fn encode(&self, encoder: MetricEncoder) -> Result<(), fmt::Error> {
        (self.pick)(&(self.snapshot)(&self.source)).encode(encoder, self.counter)
    }

    fn metric_type(&self) -> MetricType {
        if self.counter {
            MetricType::Counter
        } else {
            MetricType::Gauge
        }
    }
}

/// Регистрирует показатели `defs` источника `source`.
fn register<T, S, V, const N: usize>(
    metrics: &Metrics,
    source: &Arc<T>,
    snapshot: fn(&T) -> S,
    defs: [StatDef<S, V>; N],
) where
    T: Send + Sync + 'static,
    S: 'static,
    V: StatValue,
{
    for (name, help, pick, counter) in defs {
        metrics.register(
            name,
            help,
            Stat {
                source: source.clone(),
                snapshot,
                pick,
                counter,
            },
        );
    }
}

/// Регистрирует `tp_search_cache_{hits,misses,refreshes,evictions}_total`, `tp_search_cache_entries`,
/// `tp_search_cache_bytes` (SRCH-06).
pub fn register_search_cache(metrics: &Metrics, cache: Arc<IndexCache>) {
    let defs: [StatDef<CacheStats>; 6] = [
        (
            "tp_search_cache_hits",
            "Индекс ящика открыт и актуален",
            |s| s.hits,
            true,
        ),
        (
            "tp_search_cache_misses",
            "Холодная загрузка индекса из BlobStore",
            |s| s.misses,
            true,
        ),
        (
            "tp_search_cache_refreshes",
            "Догрузка изменений отставшего индекса",
            |s| s.refreshes,
            true,
        ),
        (
            "tp_search_cache_evictions",
            "Индексы, вытесненные по размеру кэша",
            |s| s.evictions,
            true,
        ),
        (
            "tp_search_cache_entries",
            "Индексов в кэше узла",
            |s| s.entries,
            false,
        ),
        (
            "tp_search_cache_bytes",
            "Размер индексов в кэше узла, байт",
            |s| s.bytes,
            false,
        ),
    ];
    register(metrics, &cache, IndexCache::stats, defs);
}

/// Регистрирует `tp_search_indexer_failures_total` и `tp_search_indexer_retrying` (CMP-06, CMP-09).
pub fn register_indexer(metrics: &Metrics, indexer: Arc<Indexer>) {
    let defs: [StatDef<IndexerStats>; 2] = [
        (
            "tp_search_indexer_failures",
            "Пачки журнала событий, обработка которых завершилась ошибкой",
            |s| s.failures,
            true,
        ),
        (
            "tp_search_indexer_retrying",
            "Разделы журнала, ждущие повтора пачки после ошибки",
            |s| s.retrying,
            false,
        ),
    ];
    register(metrics, &indexer, Indexer::stats, defs);
}

/// Регистрирует метрики переключения СУБД (META-25, SRS 3.1): `tp_db_failover_{switches,retries,gave_up}_total`,
/// `tp_db_failover_switch_seconds_total`, `tp_db_failover_last_switch_seconds`, `tp_db_failover_switching`,
/// `tp_db_failover_primary`.
pub fn register_failover(metrics: &Metrics, failover: Failover) {
    let source = Arc::new(failover);
    let counts: [StatDef<FailoverMetrics>; 5] = [
        (
            "tp_db_failover_switches",
            "Переключения пула СУБД на найденный primary",
            |m| m.switches,
            true,
        ),
        (
            "tp_db_failover_retries",
            "Повторы операций СУБД после временного отказа",
            |m| m.retries,
            true,
        ),
        (
            "tp_db_failover_gave_up",
            "Операции СУБД, не выполненные за database.failover.retry_for",
            |m| m.gave_up,
            true,
        ),
        (
            "tp_db_failover_switching",
            "Идёт переключение СУБД: primary недоступен для записи, новый не найден",
            |m| u64::from(m.switching),
            false,
        ),
        (
            "tp_db_failover_primary",
            "Номер текущего primary в списке database.url + database.urls (0 — url)",
            |m| u64::try_from(m.primary).unwrap_or(u64::MAX),
            false,
        ),
    ];
    register(metrics, &source, Failover::metrics, counts);
    let seconds: [StatDef<FailoverMetrics, f64>; 2] = [
        (
            "tp_db_failover_switch_seconds",
            "Суммарная длительность переключений СУБД, с",
            |m| m.switch_total.as_secs_f64(),
            true,
        ),
        (
            "tp_db_failover_last_switch_seconds",
            "Длительность последнего переключения СУБД, с (0 — переключений не было)",
            |m| m.last_switch.map_or(0.0, |d| d.as_secs_f64()),
            false,
        ),
    ];
    register(metrics, &source, Failover::metrics, seconds);
}

/// Имя пула узла в метке `pool`.
pub const POOL_PRIMARY: &str = "primary";
/// Имя слушателя сигналов узла в метке `listener`.
pub const LISTENER_SIGNALS: &str = "signals";

/// Гистограмма длительностей из снимка `tp-meta` ([`Histogram`]): корзины — как у источника (64 мкс × 2^i).
struct Durations<F>(F);

impl<F> fmt::Debug for Durations<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Durations")
    }
}

impl<F: Fn() -> Histogram + Send + Sync> EncodeMetric for Durations<F> {
    fn encode(&self, mut encoder: MetricEncoder) -> Result<(), fmt::Error> {
        let h = (self.0)();
        // Кодировщик сам накапливает корзины (`le`); последняя граница — `+Inf`.
        let mut buckets = [(f64::MAX, 0u64); HISTOGRAM_BUCKETS];
        for (i, (slot, n)) in buckets.iter_mut().zip(h.buckets).enumerate() {
            *slot = (
                Histogram::upper_bound(i).map_or(f64::MAX, |d| d.as_secs_f64()),
                n,
            );
        }
        encoder.encode_histogram::<NoLabelSet>(h.sum.as_secs_f64(), h.count, &buckets, None)
    }

    fn metric_type(&self) -> MetricType {
        MetricType::Histogram
    }
}

/// Регистрирует метрики СУБД узла для критериев Д.7 (APP-34) в наборе `db` (`DbMetrics::register`):
/// - пул узла `failover` — `tp_db_pool_{in_use,idle,max}{pool="primary"}`; возвращается наблюдатель ожидания
///   соединения (`tp_db_pool_waiting`, `tp_db_pool_wait_seconds`, `tp_db_pool_acquire_errors_total`) для передачи
///   хранилищу метаданных (META-55);
/// - шина LISTEN/NOTIFY `signals` (`bus.kind: postgres`) — слушатель `tp_db_listener_*{listener="signals"}`,
///   `tp_db_notify_commit_seconds` (время фиксаций с NOTIFY), `tp_db_commit_seconds` (время всех фиксаций),
///   `tp_db_listeners` (соединений-слушателей узла), `tp_db_notify_commit_share` (доля NOTIFY во времени фиксаций,
///   критерий — не больше 0,05; 0 — фиксаций ещё не было).
pub fn register_db(
    metrics: &Metrics,
    db: &DbMetrics,
    failover: &Failover,
    signals: Option<PgSignalBus>,
) -> Arc<dyn AcquireObserver> {
    let pool = failover.pool().clone();
    let observer = db.pool_observer(POOL_PRIMARY, move || PoolSample {
        size: pool.size(),
        idle: u32::try_from(pool.num_idle()).unwrap_or(u32::MAX),
        max: pool.options().get_max_connections(),
    });
    let Some(bus) = signals else {
        return observer;
    };
    let listener = bus.clone();
    db.listener(LISTENER_SIGNALS, move || {
        let m = listener.metrics();
        ListenerSample {
            connected: listener.connected(),
            subscribers: u64::try_from(listener.subscribers()).unwrap_or(u64::MAX),
            reconnects: m.reconnects,
            received: m.received,
            lag: m.latency_last,
        }
    });
    let source = bus.clone();
    metrics.register(
        "tp_db_notify_commit_seconds",
        "Время фиксаций с NOTIFY (отправки публикатора сигналов), с",
        Durations(move || source.metrics().notify_commit),
    );
    let source = bus.clone();
    metrics.register(
        "tp_db_commit_seconds",
        "Время всех фиксаций процесса на primary, включая NOTIFY, с",
        Durations(move || source.metrics().commit),
    );
    let source = Arc::new(bus);
    let listeners: [StatDef<SignalMetrics>; 1] = [(
        "tp_db_listeners",
        "Соединений-слушателей LISTEN узла",
        |m| m.listeners,
        false,
    )];
    register(metrics, &source, PgSignalBus::metrics, listeners);
    let share: [StatDef<SignalMetrics, f64>; 1] = [(
        "tp_db_notify_commit_share",
        "Доля времени фиксаций, занятая NOTIFY (0 — фиксаций ещё не было)",
        |m| m.notify_commit_share().unwrap_or(0.0),
        false,
    )];
    register(metrics, &source, PgSignalBus::metrics, share);
    observer
}

/// Регистрирует метрики поиска «сирот» BLOB роли maintenance (BLOB-16, Д.7):
/// `tp_blob_orphans_{passes,found,deleted}_total`, `tp_blob_orphans_remaining`. Возвращает держатель воркера для
/// `Maintenance::orphan_probe`: до запуска роли значения нулевые.
pub fn register_orphans(metrics: &Metrics) -> OrphanProbe {
    let probe = OrphanProbe::default();
    register(
        metrics,
        &Arc::new(probe.clone()),
        OrphanProbe::metrics,
        OrphanMetrics::STATS,
    );
    probe
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Гистограмма из снимка `tp-meta` кодируется накопительно с границами в секундах и `+Inf`.
    #[test]
    fn durations_histogram_encoded() {
        let metrics = Metrics::default();
        let mut h = Histogram::default();
        h.buckets[0] = 2;
        h.buckets[1] = 1;
        h.buckets[HISTOGRAM_BUCKETS - 1] = 1;
        h.count = 4;
        h.sum = std::time::Duration::from_millis(1500);
        metrics.register("tp_test_seconds", "test", Durations(move || h));
        let text = metrics.encode().expect("encode");
        assert!(text.contains("# TYPE tp_test_seconds histogram"), "{text}");
        assert!(text.contains("tp_test_seconds_sum 1.5"), "{text}");
        assert!(text.contains("tp_test_seconds_count 4"), "{text}");
        assert!(
            text.contains("tp_test_seconds_bucket{le=\"0.000064\"} 2"),
            "{text}"
        );
        assert!(
            text.contains("tp_test_seconds_bucket{le=\"0.000128\"} 3"),
            "{text}"
        );
        assert!(
            text.contains("tp_test_seconds_bucket{le=\"+Inf\"} 4"),
            "{text}"
        );
    }

    /// «Сироты»: до запуска роли maintenance показатели есть и равны нулю.
    #[test]
    fn orphan_stats_exported_before_role_start() {
        let metrics = Metrics::default();
        let probe = register_orphans(&metrics);
        assert_eq!(probe.metrics(), OrphanMetrics::default());
        let text = metrics.encode().expect("encode");
        for line in [
            "tp_blob_orphans_passes_total 0",
            "tp_blob_orphans_found_total 0",
            "tp_blob_orphans_deleted_total 0",
            "tp_blob_orphans_remaining 0",
        ] {
            assert!(text.contains(line), "{line}: {text}");
        }
    }

    #[test]
    fn indexer_stats_exported() {
        let metrics = Metrics::default();
        let indexer = Arc::new(Indexer::new(
            Arc::new(tp_search::IndexManager::new(Default::default())),
            Arc::new(NoDocuments),
            Default::default(),
        ));
        register_indexer(&metrics, indexer);
        let text = metrics.encode().expect("encode");
        assert!(
            text.contains("tp_search_indexer_failures_total 0"),
            "{text}"
        );
        assert!(text.contains("tp_search_indexer_retrying 0"), "{text}");
    }

    struct NoDocuments;

    #[async_trait::async_trait]
    impl tp_search::DocumentSource for NoDocuments {
        async fn document(
            &self,
            _: tp_contracts::meta::AccountScope,
            _: tp_contracts::MessageId,
        ) -> tp_contracts::Result<Option<tp_search::IndexDoc>> {
            Ok(None)
        }

        async fn messages(
            &self,
            _: tp_contracts::meta::AccountScope,
        ) -> tp_contracts::Result<Vec<tp_contracts::MessageId>> {
            Ok(Vec::new())
        }
    }
}
