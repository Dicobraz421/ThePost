//! Кэш индексов на NVMe узла (SRS 4.6): LRU по размеру, холодная загрузка из BlobStore, метрики.
//!
//! Индекс ящика на узле — каталог в [`IndexConfig::dir`](crate::IndexConfig::dir). [`IndexCache::get`] отдаёт
//! индекс по манифесту: открытый и проверенный не раньше [`CacheConfig::refresh_interval`] — без обращения
//! к метаданным (попадание); отставший от манифеста догружается (обновление); отсутствующий загружается из
//! BlobStore (промах, холодная загрузка). Учёт размера и вытеснение — `moka` (политика LRU, вес — размер
//! индекса по манифесту): вытесненный индекс закрывается и его каталог удаляется; запросы, уже получившие
//! индекс, дорабатывают по открытым файлам.
//!
//! Вызовы `moka` выполняются без блокировки индекса ([`IndexManager::lock`]): обработчик вытеснения
//! захватывает её сам.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moka::future::Cache;
use moka::notification::{ListenerFuture, RemovalCause};
use moka::policy::EvictionPolicy;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::time::Instant;
use tp_contracts::config::duration;
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Result, TenantId};

use crate::index::{IndexManager, MailIndex, blocking, lock};
use crate::persist::IndexStore;

/// Кэш индексов (секция `search.cache`; поля необязательны, неизвестное — ошибка).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Предел суммарного размера индексов на диске узла, байт; сверх него вытесняются давно не
    /// использованные.
    pub max_bytes: u64,
    /// Как долго индекс считается актуальным без сверки с манифестом (`5s`): задержка появления
    /// изменений другого узла в поиске.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub refresh_interval: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: 64 << 30,
            refresh_interval: Duration::from_secs(5),
        }
    }
}

/// Счётчики кэша (с запуска узла).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// Индекс открыт и актуален.
    pub hits: u64,
    /// Индекса не было на узле: холодная загрузка из BlobStore.
    pub misses: u64,
    /// Индекс отставал от манифеста: догружены изменения.
    pub refreshes: u64,
    /// Вытеснено по размеру.
    pub evictions: u64,
    /// Индексов в кэше.
    pub entries: u64,
    /// Их суммарный размер по манифестам, байт (с точностью до КиБ на индекс).
    pub bytes: u64,
}

#[derive(Debug, Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    refreshes: AtomicU64,
    evictions: AtomicU64,
}

type Key = (TenantId, AccountId);

/// Вес индекса в кэше: размер в КиБ (не меньше 1).
fn weight(bytes: u64) -> u32 {
    u32::try_from(bytes.div_ceil(1024).max(1)).unwrap_or(u32::MAX)
}

/// Индексы ящиков на узле с загрузкой из BlobStore и вытеснением по размеру.
pub struct IndexCache {
    indexes: Arc<IndexManager>,
    store: Arc<IndexStore>,
    cfg: CacheConfig,
    lru: Cache<Key, u32>,
    counters: Arc<Counters>,
    checked: Mutex<HashMap<Key, Instant>>,
}

impl std::fmt::Debug for IndexCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexCache")
            .field("cfg", &self.cfg)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl IndexCache {
    /// Кэш индексов `indexes`, загружаемых из `store`.
    pub fn new(indexes: Arc<IndexManager>, store: Arc<IndexStore>, cfg: CacheConfig) -> Self {
        let counters = Arc::new(Counters::default());
        let listener = {
            let (indexes, counters) = (indexes.clone(), counters.clone());
            move |key: Arc<Key>, _: u32, cause: RemovalCause| -> ListenerFuture {
                let (indexes, counters) = (indexes.clone(), counters.clone());
                Box::pin(async move {
                    if cause != RemovalCause::Size {
                        return;
                    }
                    let (tenant, account) = *key;
                    let scope = AccountScope { tenant, account };
                    let _gate = indexes.lock(scope).await;
                    let m = indexes.clone();
                    match blocking(move || m.evict(scope)).await {
                        Ok(()) => {
                            counters.evictions.fetch_add(1, Ordering::Relaxed);
                            tracing::debug!(tenant = %tenant, account = %account, "search index evicted");
                        }
                        Err(e) => tracing::warn!(tenant = %tenant, account = %account, error = %e,
                            "search index eviction failed"),
                    }
                })
            }
        };
        let lru = Cache::builder()
            .max_capacity(cfg.max_bytes.div_ceil(1024))
            .weigher(|_, w: &u32| *w)
            .eviction_policy(EvictionPolicy::lru())
            .async_eviction_listener(listener)
            .build();
        Self {
            indexes,
            store,
            cfg,
            lru,
            counters,
            checked: Mutex::new(HashMap::new()),
        }
    }

    /// Хранилище сегментов кэша.
    pub fn store(&self) -> &Arc<IndexStore> {
        &self.store
    }

    /// Индекс учётной записи по манифесту; индекса нет — `None`.
    pub async fn get(&self, scope: AccountScope) -> Result<Option<Arc<MailIndex>>> {
        let key = (scope.tenant, scope.account);
        if let Some(index) = self.indexes.cached(scope)
            && lock(&self.checked)
                .get(&key)
                .is_some_and(|t| t.elapsed() < self.cfg.refresh_interval)
        {
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
            self.lru.get(&key).await;
            return Ok(Some(index));
        }
        let index = {
            let _gate = self.indexes.lock(scope).await;
            let before = self.indexes.cached(scope).map(|i| i.synced().segments);
            let index = self.store.sync(&self.indexes, scope, false).await?;
            if let Some(index) = &index {
                let counter = match before {
                    None => &self.counters.misses,
                    Some(segments) if segments != index.synced().segments => {
                        &self.counters.refreshes
                    }
                    Some(_) => &self.counters.hits,
                };
                counter.fetch_add(1, Ordering::Relaxed);
                lock(&self.checked).insert(key, Instant::now());
            }
            index
        };
        match &index {
            Some(index) => self.touch(scope, index).await,
            None => self.lru.invalidate(&key).await,
        }
        Ok(index)
    }

    /// Учитывает индекс в кэше (после записи индексатором на этом узле): размер — по манифесту.
    pub async fn touch(&self, scope: AccountScope, index: &MailIndex) {
        let key = (scope.tenant, scope.account);
        self.lru.insert(key, weight(index.synced().size())).await;
        self.lru.run_pending_tasks().await;
    }

    /// Счётчики и заполнение кэша.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.counters.hits.load(Ordering::Relaxed),
            misses: self.counters.misses.load(Ordering::Relaxed),
            refreshes: self.counters.refreshes.load(Ordering::Relaxed),
            evictions: self.counters.evictions.load(Ordering::Relaxed),
            entries: self.lru.entry_count(),
            bytes: self.lru.weighted_size().saturating_mul(1024),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::scope;
    use crate::persist::tests::{apply, count, mail, node, store, upserts};
    use crate::{QueryConfig, Search};

    #[tokio::test]
    async fn lru_eviction_and_cold_load() {
        let (_, _, store) = store();
        let writer = node();
        let scopes = [scope(), scope(), scope()];
        let mut weights = Vec::new();
        for (i, scope) in scopes.iter().enumerate() {
            let index = writer.indexes.open_or_create(*scope).expect("create");
            let docs: Vec<_> = (0..10)
                .map(|n| mail(&format!("Ящик{i} письмо {n}")))
                .collect();
            apply(&index, upserts(&docs)).await;
            let m = store.publish(*scope, &index).await.expect("publish");
            weights.push(u64::from(weight(m.size())));
        }
        // Помещаются любые два индекса, три — нет.
        let reader = node();
        let cache = Arc::new(IndexCache::new(
            reader.indexes.clone(),
            store.clone(),
            CacheConfig {
                max_bytes: (weights.iter().sum::<u64>() - 1) * 1024,
                refresh_interval: Duration::from_secs(3600),
            },
        ));
        let search =
            Search::new(reader.indexes.clone(), QueryConfig::default()).with_cache(cache.clone());
        let [s0, s1, s2] = scopes;

        assert_eq!(count(&search, s0, "ящик0").await, 10);
        assert_eq!(count(&search, s1, "ящик1").await, 10);
        assert_eq!(count(&search, s0, "письмо").await, 10);
        let st = cache.stats();
        assert_eq!((st.misses, st.hits, st.evictions, st.entries), (2, 1, 0, 2));
        assert_eq!(st.bytes, (weights[0] + weights[1]) * 1024);

        // Третий индекс вытесняет давно не использованный (s1): каталог удаляется.
        assert_eq!(count(&search, s2, "ящик2").await, 10);
        let st = cache.stats();
        assert_eq!((st.misses, st.evictions, st.entries), (3, 1, 2));
        assert!(!reader.indexes.path(s1).exists());
        assert!(reader.indexes.cached(s1).is_none());
        assert!(reader.indexes.path(s0).exists() && reader.indexes.path(s2).exists());

        // Вытесненный загружается заново (холодная загрузка), вытесняя следующий по давности (s0).
        assert_eq!(count(&search, s1, "ящик1").await, 10);
        let st = cache.stats();
        assert_eq!((st.misses, st.evictions), (4, 2));
        assert!(!reader.indexes.path(s0).exists());

        // Ящик без индекса — пустой результат, в кэш не попадает.
        assert_eq!(count(&search, scope(), "письмо").await, 0);
        assert_eq!(cache.stats().entries, 2);
    }

    #[tokio::test]
    async fn refresh_after_interval() {
        let (_, _, store) = store();
        let (writer, reader) = (node(), node());
        let scope = scope();
        let index = writer.indexes.open_or_create(scope).expect("create");
        apply(&index, upserts(&[mail("Первое письмо")])).await;
        store.publish(scope, &index).await.expect("publish");
        let cache = Arc::new(IndexCache::new(
            reader.indexes.clone(),
            store.clone(),
            CacheConfig {
                refresh_interval: Duration::ZERO,
                ..CacheConfig::default()
            },
        ));
        let search =
            Search::new(reader.indexes.clone(), QueryConfig::default()).with_cache(cache.clone());
        assert_eq!(count(&search, scope, "письмо").await, 1);
        assert_eq!(count(&search, scope, "письмо").await, 1);
        apply(&index, upserts(&[mail("Второе письмо")])).await;
        store.publish(scope, &index).await.expect("publish 2");
        assert_eq!(count(&search, scope, "письмо").await, 2);
        let st = cache.stats();
        assert_eq!((st.misses, st.hits, st.refreshes), (1, 1, 1));
    }

    #[test]
    fn config_defaults_and_unknown() {
        assert_eq!(
            crate::tests::from_pairs::<CacheConfig>(&[]).expect("defaults"),
            CacheConfig::default()
        );
        let cfg =
            crate::tests::from_pairs::<CacheConfig>(&[("refresh_interval", "1s")]).expect("1s");
        assert_eq!(cfg.refresh_interval, Duration::from_secs(1));
        assert!(crate::tests::from_pairs::<CacheConfig>(&[("max_size", "1")]).is_err());
        assert_eq!(weight(0), 1);
        assert_eq!(weight(1025), 2);
    }
}
