//! Метрики сервисов приложения, которые не регистрируют сами крейты (APP-04): счётчики кэша индексов поиска
//! и индексатора, переключение СУБД (APP-14). Значения читаются на каждый запрос `/metrics` — без своих таймеров.

use std::fmt;
use std::sync::Arc;

use prometheus_client::encoding::{EncodeMetric, MetricEncoder, NoLabelSet};
use prometheus_client::metrics::MetricType;
use tp_meta::failover::{Failover, FailoverMetrics};
use tp_platform::Metrics;
use tp_search::{CacheStats, IndexCache, Indexer, IndexerStats};

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

#[cfg(test)]
mod tests {
    use super::*;

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
