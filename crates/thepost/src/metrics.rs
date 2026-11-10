//! Метрики сервисов приложения, которые не регистрируют сами крейты (APP-04): счётчики кэша индексов поиска
//! и индексатора. Значения читаются на каждый запрос `/metrics` — без своих таймеров.

use std::fmt;
use std::sync::Arc;

use prometheus_client::encoding::{EncodeMetric, MetricEncoder, NoLabelSet};
use prometheus_client::metrics::MetricType;
use tp_platform::Metrics;
use tp_search::{CacheStats, IndexCache, Indexer, IndexerStats};

/// Описание показателя: имя, пояснение, значение из снимка `S`, счётчик (иначе — текущее значение).
type StatDef<S> = (&'static str, &'static str, fn(&S) -> u64, bool);

/// Показатель источника `T` со снимком `S`: счётчик или текущее значение.
struct Stat<T, S> {
    source: Arc<T>,
    snapshot: fn(&T) -> S,
    pick: fn(&S) -> u64,
    counter: bool,
}

impl<T, S> fmt::Debug for Stat<T, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stat")
            .field("counter", &self.counter)
            .finish_non_exhaustive()
    }
}

impl<T: Send + Sync, S> EncodeMetric for Stat<T, S> {
    fn encode(&self, mut encoder: MetricEncoder) -> Result<(), fmt::Error> {
        let value = (self.pick)(&(self.snapshot)(&self.source));
        if self.counter {
            encoder.encode_counter::<NoLabelSet, _, u64>(&value, None)
        } else {
            encoder.encode_gauge(&i64::try_from(value).unwrap_or(i64::MAX))
        }
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
fn register<T, S, const N: usize>(
    metrics: &Metrics,
    source: &Arc<T>,
    snapshot: fn(&T) -> S,
    defs: [StatDef<S>; N],
) where
    T: Send + Sync + 'static,
    S: 'static,
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
