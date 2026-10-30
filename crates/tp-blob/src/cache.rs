//! NVMe-кэш объектов (SRS 3.2): read-through поверх источника, LRU по суммарному размеру.
//!
//! Кэш не источник истины: объекты лежат в локальном каталоге без fsync, индекс — в памяти (moka),
//! при открытии каталог очищается. Кэшируются только ключи BLOB — они неизменяемы; пути `*_raw`
//! и листинг проходят к источнику напрямую. Файл, пропавший из каталога, читается из источника.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use moka::future::Cache;
use moka::notification::RemovalCause;
use moka::policy::EvictionPolicy;
use schemars::JsonSchema;
use time::OffsetDateTime;
use tp_contracts::blob::{BlobStore, ByteStream, ObjectStream};
use tp_contracts::{BlobKey, Error, Result};

use crate::local::{LocalConfig, LocalStore};

/// Подкаталог объектов внутри каталога кэша; очищается при открытии.
const OBJECTS_DIR: &str = "objects";

/// Конфигурация кэша объектов.
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
pub struct CacheConfig {
    /// Каталог кэша на локальном NVMe.
    pub dir: PathBuf,
    /// Предел суммарного размера объектов в кэше, байт.
    pub max_bytes: u64,
    /// Объекты больше этого размера не кэшируются, байт (не больше 4 ГиБ).
    pub max_object_bytes: u64,
    /// Класть объект в кэш при записи через кэш (свежая почта читается первой).
    pub fill_on_put: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("/var/cache/thepost/blob"),
            max_bytes: 10 << 30,
            max_object_bytes: 64 << 20,
            fill_on_put: true,
        }
    }
}

/// Причина, по которой объект не попал в кэш.
#[derive(Debug, thiserror::Error)]
enum FillError {
    #[error("object exceeds cache limit")]
    TooLarge,
    #[error(transparent)]
    Store(#[from] Error),
}

/// Read-through кэш поверх `source`; сам является [`BlobStore`].
pub struct BlobCache {
    source: Arc<dyn BlobStore>,
    disk: Arc<LocalStore>,
    /// Ключ → размер объекта в кэше; вес — размер, вытеснение удаляет файл.
    index: Cache<BlobKey, u64>,
    cfg: CacheConfig,
}

impl std::fmt::Debug for BlobCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobCache")
            .field("cfg", &self.cfg)
            .field("entries", &self.index.entry_count())
            .field("bytes", &self.index.weighted_size())
            .finish_non_exhaustive()
    }
}

impl BlobCache {
    /// Открывает кэш: очищает каталог объектов и начинает с пустого индекса.
    pub async fn open(cfg: CacheConfig, source: Arc<dyn BlobStore>) -> Result<Self> {
        let root = cfg.dir.join(OBJECTS_DIR);
        if let Err(e) = tokio::fs::remove_dir_all(&root).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e.into());
        }
        let disk = Arc::new(
            LocalStore::open(LocalConfig {
                root,
                fsync: false,
                ..LocalConfig::default()
            })
            .await?,
        );
        let evicted = disk.clone();
        let index = Cache::builder()
            .max_capacity(cfg.max_bytes)
            .weigher(|_, size: &u64| u32::try_from(*size).unwrap_or(u32::MAX))
            .eviction_policy(EvictionPolicy::lru())
            .async_eviction_listener(move |key: Arc<BlobKey>, _, cause| {
                let disk = evicted.clone();
                Box::pin(async move {
                    // Замена записи — тот же файл, переписанный заново.
                    if cause == RemovalCause::Replaced {
                        return;
                    }
                    if let Err(e) = disk.delete(&key).await {
                        tracing::warn!(error = %e, "blob cache: evicted file not removed");
                    }
                })
            })
            .build();
        Ok(Self {
            source,
            disk,
            index,
            cfg,
        })
    }

    /// Загружает объект из источника в каталог кэша; возвращает размер.
    async fn fill(&self, key: &BlobKey) -> std::result::Result<u64, FillError> {
        let size = self.source.head(key).await?.ok_or(Error::NotFound)?;
        if size > self.cfg.max_object_bytes {
            return Err(FillError::TooLarge);
        }
        let data = self.source.get(key, None).await?;
        self.disk.put_stream(key, data, Some(size)).await?;
        Ok(size)
    }
}

#[async_trait]
impl BlobStore for BlobCache {
    async fn put(&self, key: &BlobKey, data: Bytes) -> Result<()> {
        self.source.put(key, data.clone()).await?;
        let size = data.len() as u64;
        if self.cfg.fill_on_put && size <= self.cfg.max_object_bytes {
            match self.disk.put(key, data).await {
                Ok(()) => self.index.insert(*key, size).await,
                Err(e) => tracing::warn!(error = %e, "blob cache: object not cached"),
            }
        }
        Ok(())
    }

    async fn put_stream(
        &self,
        key: &BlobKey,
        data: ByteStream,
        size_hint: Option<u64>,
    ) -> Result<()> {
        self.source.put_stream(key, data, size_hint).await
    }

    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream> {
        match self.index.try_get_with(*key, self.fill(key)).await {
            Ok(_) => match self.disk.get(key, range.clone()).await {
                Err(Error::NotFound) => {
                    self.index.invalidate(key).await;
                    self.source.get(key, range).await
                }
                res => res,
            },
            Err(e) => match &*e {
                FillError::TooLarge => self.source.get(key, range).await,
                FillError::Store(e) => Err(e.clone()),
            },
        }
    }

    async fn head(&self, key: &BlobKey) -> Result<Option<u64>> {
        match self.index.get(key).await {
            Some(size) => Ok(Some(size)),
            None => self.source.head(key).await,
        }
    }

    async fn delete(&self, key: &BlobKey) -> Result<()> {
        self.source.delete(key).await?;
        self.index.invalidate(key).await;
        Ok(())
    }

    fn list(&self, prefix: &str, older_than: OffsetDateTime) -> ObjectStream {
        self.source.list(prefix, older_than)
    }

    async fn put_raw(&self, path: &str, data: Bytes) -> Result<()> {
        self.source.put_raw(path, data).await
    }

    async fn get_raw(&self, path: &str, range: Option<Range<u64>>) -> Result<ByteStream> {
        self.source.get_raw(path, range).await
    }

    async fn delete_raw(&self, path: &str) -> Result<()> {
        self.source.delete_raw(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tp_contracts::{BlobHash, Gen, TenantId};

    /// Источник со счётчиком обращений на чтение.
    struct Counting {
        inner: LocalStore,
        reads: AtomicUsize,
    }

    impl Counting {
        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl BlobStore for Counting {
        async fn put(&self, key: &BlobKey, data: Bytes) -> Result<()> {
            self.inner.put(key, data).await
        }
        async fn put_stream(
            &self,
            key: &BlobKey,
            data: ByteStream,
            size_hint: Option<u64>,
        ) -> Result<()> {
            self.inner.put_stream(key, data, size_hint).await
        }
        async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.get(key, range).await
        }
        async fn head(&self, key: &BlobKey) -> Result<Option<u64>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.head(key).await
        }
        async fn delete(&self, key: &BlobKey) -> Result<()> {
            self.inner.delete(key).await
        }
        fn list(&self, prefix: &str, older_than: OffsetDateTime) -> ObjectStream {
            self.inner.list(prefix, older_than)
        }
        async fn put_raw(&self, path: &str, data: Bytes) -> Result<()> {
            self.inner.put_raw(path, data).await
        }
        async fn get_raw(&self, path: &str, range: Option<Range<u64>>) -> Result<ByteStream> {
            self.inner.get_raw(path, range).await
        }
        async fn delete_raw(&self, path: &str) -> Result<()> {
            self.inner.delete_raw(path).await
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        source: Arc<Counting>,
        cache: BlobCache,
    }

    async fn fixture(max_bytes: u64, max_object_bytes: u64) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = LocalStore::open(LocalConfig {
            root: dir.path().join("source"),
            ..LocalConfig::default()
        })
        .await
        .expect("source");
        let source = Arc::new(Counting {
            inner,
            reads: AtomicUsize::new(0),
        });
        let cache = BlobCache::open(
            CacheConfig {
                dir: dir.path().join("cache"),
                max_bytes,
                max_object_bytes,
                fill_on_put: false,
            },
            source.clone(),
        )
        .await
        .expect("cache");
        Fixture {
            _dir: dir,
            source,
            cache,
        }
    }

    fn key(n: u8) -> BlobKey {
        BlobKey {
            tenant: TenantId::DEFAULT,
            hash: BlobHash([n; 32]),
            generation: Gen(1),
        }
    }

    async fn read(cache: &BlobCache, key: &BlobKey, range: Option<Range<u64>>) -> Vec<u8> {
        let chunks: Vec<Bytes> = cache
            .get(key, range)
            .await
            .expect("get")
            .try_collect()
            .await
            .expect("read");
        chunks.concat()
    }

    #[tokio::test]
    async fn repeated_read_skips_source() {
        let f = fixture(1 << 20, 1 << 20).await;
        let k = key(1);
        f.source
            .put(&k, Bytes::from_static(b"0123456789"))
            .await
            .expect("put");

        assert_eq!(read(&f.cache, &k, None).await, b"0123456789");
        let after_miss = f.source.reads();
        assert_eq!(read(&f.cache, &k, None).await, b"0123456789");
        assert_eq!(read(&f.cache, &k, Some(2..5)).await, b"234");
        assert_eq!(f.cache.head(&k).await, Ok(Some(10)));
        assert_eq!(f.source.reads(), after_miss);
    }

    #[tokio::test]
    async fn evicts_by_total_size() {
        let f = fixture(100, 100).await;
        let keys = [key(1), key(2), key(3)];
        for k in &keys {
            f.source
                .put(k, Bytes::from(vec![k.hash.0[0]; 40]))
                .await
                .expect("put");
            read(&f.cache, k, None).await;
        }
        f.cache.index.run_pending_tasks().await;
        assert!(f.cache.index.weighted_size() <= 100);
        // Вытеснен самый давний — key(1): его файл удалён, чтение снова идёт в источник.
        assert!(!f.cache.disk.root().join(keys[0].path()).exists());
        assert!(f.cache.disk.root().join(keys[2].path()).exists());
        let reads = f.source.reads();
        assert_eq!(read(&f.cache, &keys[0], None).await, vec![1u8; 40]);
        assert!(f.source.reads() > reads);
    }

    #[tokio::test]
    async fn large_objects_bypass_cache() {
        let f = fixture(1 << 20, 4).await;
        let k = key(4);
        f.source
            .put(&k, Bytes::from_static(b"too large"))
            .await
            .expect("put");
        assert_eq!(read(&f.cache, &k, None).await, b"too large");
        let reads = f.source.reads();
        assert_eq!(read(&f.cache, &k, Some(4..9)).await, b"large");
        assert!(f.source.reads() > reads);
        assert!(!f.cache.disk.root().join(k.path()).exists());
    }

    #[tokio::test]
    async fn missing_object_not_cached() {
        let f = fixture(1 << 20, 1 << 20).await;
        let k = key(5);
        assert!(matches!(f.cache.get(&k, None).await, Err(Error::NotFound)));
        f.source
            .put(&k, Bytes::from_static(b"late"))
            .await
            .expect("put");
        assert_eq!(read(&f.cache, &k, None).await, b"late");
    }

    #[tokio::test]
    async fn delete_and_lost_file() {
        let f = fixture(1 << 20, 1 << 20).await;
        let k = key(6);
        f.source
            .put(&k, Bytes::from_static(b"data"))
            .await
            .expect("put");
        read(&f.cache, &k, None).await;

        // Файл кэша пропал (очистка каталога) — чтение идёт в источник.
        std::fs::remove_file(f.cache.disk.root().join(k.path())).expect("remove");
        assert_eq!(read(&f.cache, &k, None).await, b"data");

        f.cache.delete(&k).await.expect("delete");
        assert!(matches!(f.cache.get(&k, None).await, Err(Error::NotFound)));
        assert_eq!(f.cache.head(&k).await, Ok(None));
    }

    #[tokio::test]
    async fn put_fills_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = LocalStore::open(LocalConfig {
            root: dir.path().join("source"),
            ..LocalConfig::default()
        })
        .await
        .expect("source");
        let source = Arc::new(Counting {
            inner,
            reads: AtomicUsize::new(0),
        });
        let cache = BlobCache::open(
            CacheConfig {
                dir: dir.path().join("cache"),
                ..CacheConfig::default()
            },
            source.clone(),
        )
        .await
        .expect("cache");
        let k = key(7);
        cache
            .put(&k, Bytes::from_static(b"fresh"))
            .await
            .expect("put");
        assert_eq!(read(&cache, &k, None).await, b"fresh");
        assert_eq!(source.reads(), 0);
    }
}
