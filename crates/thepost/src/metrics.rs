//! Метрики сервисов приложения, которые не регистрируют сами крейты (APP-04): счётчики кэша индексов поиска.
//! Значения читаются на каждый запрос `/metrics` — без своих таймеров.

use std::fmt;
use std::sync::Arc;

use prometheus_client::encoding::{EncodeMetric, MetricEncoder, NoLabelSet};
use prometheus_client::metrics::MetricType;
use tp_platform::Metrics;
use tp_search::{CacheStats, IndexCache};

/// Описание показателя кэша: имя, пояснение, значение из снимка, счётчик (иначе — текущее значение).
type StatDef = (&'static str, &'static str, fn(&CacheStats) -> u64, bool);

/// Показатель кэша индексов: счётчик или текущее значение.
struct CacheStat {
    cache: Arc<IndexCache>,
    pick: fn(&CacheStats) -> u64,
    counter: bool,
}

impl fmt::Debug for CacheStat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CacheStat")
            .field("counter", &self.counter)
            .finish_non_exhaustive()
    }
}

impl EncodeMetric for CacheStat {
    fn encode(&self, mut encoder: MetricEncoder) -> Result<(), fmt::Error> {
        let value = (self.pick)(&self.cache.stats());
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

/// Регистрирует `tp_search_cache_{hits,misses,refreshes,evictions}_total`, `tp_search_cache_entries`,
/// `tp_search_cache_bytes` (SRCH-06).
pub fn register_search_cache(metrics: &Metrics, cache: Arc<IndexCache>) {
    let stats: [StatDef; 6] = [
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
    for (name, help, pick, counter) in stats {
        metrics.register(
            name,
            help,
            CacheStat {
                cache: cache.clone(),
                pick,
                counter,
            },
        );
    }
}
