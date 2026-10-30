//! NVMe-кэш объектов (SRS 3.2): read-through поверх источника, LRU по суммарному размеру.
//!
//! Кэш не источник истины: объекты лежат в локальном каталоге без fsync, индекс — в памяти (moka),
//! при открытии каталог очищается. Кэшируются ключи BLOB и сегменты индексов (`*_raw`): и те, и другие
//! неизменяемы (CONTRACTS.md#BlobStore, решение 2026-10-30 #35), поэтому записи не инвалидируются —
//! только удаление через кэш и вытеснение. Сегменты лежат в подкаталоге `raw/` — не пересекаются
//! с путями ключей (`{tenant}/…`). Листинг проходит к источнику напрямую. Объект больше
//! `max_object_bytes` читается из источника; индекс помнит это, чтобы не пробовать снова.
//! Файл, пропавший из каталога, читается из источника.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use moka::future::Cache;
use moka::notification::RemovalCause;
use moka::policy::EvictionPolicy;
use schemars::JsonSchema;
use time::OffsetDateTime;
use tp_contracts::blob::{BlobStore, ByteStream, ObjectStream};
use tp_contracts::{BlobKey, Error, Result};

use crate::check_raw_path;
use crate::local::{LocalConfig, LocalStore};

/// Подкаталог объектов внутри каталога кэша; очищается при открытии.
const OBJECTS_DIR: &str = "objects";
/// Подкаталог сегментов `*_raw` внутри каталога объектов.
const RAW_DIR: &str = "raw";

/// Конфигурация кэша объектов.
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
pub struct CacheConfig {
    /// Каталог кэша на локальном NVMe.
    pub dir: PathBuf,
    /// Предел суммарного размера объектов в кэше, байт.
    pub max_bytes: u64,
    /// Объекты и сегменты больше этого размера не кэшируются, байт (не больше 4 ГиБ).
    pub max_object_bytes: u64,
    /// Класть объект или сегмент в кэш при записи через кэш (свежие данные читаются первыми).
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

/// Запись индекса кэша: объект BLOB или сегмент индекса.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Entry {
    Blob(BlobKey),
    Raw(String),
}

impl Entry {
    /// Путь файла в каталоге кэша.
    fn disk_path(&self) -> String {
        match self {
            Entry::Blob(key) => key.path(),
            Entry::Raw(path) => format!("{RAW_DIR}/{path}"),
        }
    }
}

/// Состояние записи индекса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Файл в каталоге кэша, размер в байтах.
    Disk(u64),
    /// Больше `max_object_bytes`: читать из источника (объект неизменяем — решение не устаревает).
    Bypass,
}

/// Read-through кэш поверх `source`; сам является [`BlobStore`].
pub struct BlobCache {
    source: Arc<dyn BlobStore>,
    disk: Arc<LocalStore>,
    /// Запись → состояние; вес — размер файла, вытеснение удаляет файл.
    index: Cache<Entry, Slot>,
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
            .weigher(|_, slot: &Slot| match slot {
                Slot::Disk(size) => u32::try_from(*size).unwrap_or(u32::MAX),
                Slot::Bypass => 1,
            })
            .eviction_policy(EvictionPolicy::lru())
            .async_eviction_listener(move |entry: Arc<Entry>, slot, cause| {
                let disk = evicted.clone();
                Box::pin(async move {
                    // Замена записи — тот же файл, переписанный заново.
                    if cause == RemovalCause::Replaced || slot == Slot::Bypass {
                        return;
                    }
                    if let Err(e) = disk.remove(&entry.disk_path()).await {
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

    /// Загружает объект или сегмент из источника в каталог кэша.
    async fn fill(&self, entry: &Entry) -> Result<Slot> {
        let limit = self.cfg.max_object_bytes;
        let data = match entry {
            Entry::Blob(key) => {
                let size = self.source.head(key).await?.ok_or(Error::NotFound)?;
                if size > limit {
                    return Ok(Slot::Bypass);
                }
                self.source.get(key, None).await?
            }
            // Размер сегмента заранее не известен: поток обрывается на пределе.
            Entry::Raw(path) => self.source.get_raw(path, None).await?,
        };
        let seen = Arc::new(AtomicU64::new(0));
        let counter = seen.clone();
        let limited = data
            .map(move |chunk| {
                let chunk = chunk?;
                let total =
                    counter.fetch_add(chunk.len() as u64, Ordering::Relaxed) + chunk.len() as u64;
                if total > limit {
                    return Err(Error::invalid("object exceeds cache limit"));
                }
                Ok(chunk)
            })
            .boxed();
        match self.disk.write(&entry.disk_path(), limited).await {
            Ok(()) => Ok(Slot::Disk(seen.load(Ordering::Relaxed))),
            Err(_) if seen.load(Ordering::Relaxed) > limit => Ok(Slot::Bypass),
            Err(e) => Err(e),
        }
    }

    /// Чтение из кэша с заполнением при промахе; `None` — читать из источника.
    async fn read(&self, entry: Entry, range: Option<Range<u64>>) -> Result<Option<ByteStream>> {
        let slot = self
            .index
            .try_get_with_by_ref(&entry, self.fill(&entry))
            .await
            .map_err(|e| (*e).clone())?;
        if slot == Slot::Bypass {
            return Ok(None);
        }
        match self.disk.read(&entry.disk_path(), range).await {
            Err(Error::NotFound) => {
                self.index.invalidate(&entry).await;
                Ok(None)
            }
            res => res.map(Some),
        }
    }

    /// Запись через кэш: при `fill_on_put` данные кладутся и в каталог кэша.
    async fn store(&self, entry: Entry, data: Bytes) {
        let size = data.len() as u64;
        if !self.cfg.fill_on_put || size > self.cfg.max_object_bytes {
            return;
        }
        let path = entry.disk_path();
        match self
            .disk
            .write(&path, futures::stream::iter([Ok(data)]).boxed())
            .await
        {
            Ok(()) => self.index.insert(entry, Slot::Disk(size)).await,
            Err(e) => tracing::warn!(error = %e, "blob cache: object not cached"),
        }
    }
}

#[async_trait]
impl BlobStore for BlobCache {
    async fn put(&self, key: &BlobKey, data: Bytes) -> Result<()> {
        self.source.put(key, data.clone()).await?;
        self.store(Entry::Blob(*key), data).await;
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
        match self.read(Entry::Blob(*key), range.clone()).await? {
            Some(data) => Ok(data),
            None => self.source.get(key, range).await,
        }
    }

    async fn head(&self, key: &BlobKey) -> Result<Option<u64>> {
        match self.index.get(&Entry::Blob(*key)).await {
            Some(Slot::Disk(size)) => Ok(Some(size)),
            _ => self.source.head(key).await,
        }
    }

    async fn delete(&self, key: &BlobKey) -> Result<()> {
        self.source.delete(key).await?;
        self.index.invalidate(&Entry::Blob(*key)).await;
        Ok(())
    }

    fn list(&self, prefix: &str, older_than: OffsetDateTime) -> ObjectStream {
        self.source.list(prefix, older_than)
    }

    async fn put_raw(&self, path: &str, data: Bytes) -> Result<()> {
        check_raw_path(path)?;
        self.source.put_raw(path, data.clone()).await?;
        self.store(Entry::Raw(path.to_owned()), data).await;
        Ok(())
    }

    async fn get_raw(&self, path: &str, range: Option<Range<u64>>) -> Result<ByteStream> {
        check_raw_path(path)?;
        match self
            .read(Entry::Raw(path.to_owned()), range.clone())
            .await?
        {
            Some(data) => Ok(data),
            None => self.source.get_raw(path, range).await,
        }
    }

    async fn delete_raw(&self, path: &str) -> Result<()> {
        check_raw_path(path)?;
        self.source.delete_raw(path).await?;
        self.index.invalidate(&Entry::Raw(path.to_owned())).await;
        Ok(())
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
            self.reads.fetch_add(1, Ordering::SeqCst);
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

    async fn read_raw(cache: &BlobCache, path: &str, range: Option<Range<u64>>) -> Vec<u8> {
        let chunks: Vec<Bytes> = cache
            .get_raw(path, range)
            .await
            .expect("get_raw")
            .try_collect()
            .await
            .expect("read");
        chunks.concat()
    }

    #[tokio::test]
    async fn repeated_segment_read_skips_source() {
        let f = fixture(1 << 20, 1 << 20).await;
        let seg = "idx/t/a/seg-0001";
        f.source
            .put_raw(seg, Bytes::from_static(b"segment-bytes"))
            .await
            .expect("put_raw");

        assert_eq!(read_raw(&f.cache, seg, None).await, b"segment-bytes");
        let after_miss = f.source.reads();
        assert_eq!(read_raw(&f.cache, seg, None).await, b"segment-bytes");
        assert_eq!(read_raw(&f.cache, seg, Some(8..13)).await, b"bytes");
        assert_eq!(f.source.reads(), after_miss);
        // Сегмент лежит отдельно от ключей BLOB.
        assert!(f.cache.disk.root().join(RAW_DIR).join(seg).exists());
        assert!(matches!(
            f.cache.get_raw(seg, Some(14..15)).await,
            Err(Error::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn segment_put_delete_and_paths() {
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
        let seg = "idx/t/a/seg-0002";
        cache
            .put_raw(seg, Bytes::from_static(b"fresh"))
            .await
            .expect("put_raw");
        assert_eq!(read_raw(&cache, seg, None).await, b"fresh");
        assert_eq!(source.reads(), 0);

        cache.delete_raw(seg).await.expect("delete_raw");
        assert!(matches!(
            cache.get_raw(seg, None).await,
            Err(Error::NotFound)
        ));
        for bad in ["../x", "/abs", "a//b"] {
            assert!(
                matches!(cache.get_raw(bad, None).await, Err(Error::Invalid(_))),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn large_segment_bypasses_cache() {
        let f = fixture(1 << 20, 4).await;
        let seg = "idx/t/a/seg-big";
        f.source
            .put_raw(seg, Bytes::from_static(b"too large"))
            .await
            .expect("put_raw");
        assert_eq!(read_raw(&f.cache, seg, None).await, b"too large");
        assert!(!f.cache.disk.root().join(RAW_DIR).join(seg).exists());
        // Повторно кэш не пробует: одно чтение источника на запрос.
        let reads = f.source.reads();
        assert_eq!(read_raw(&f.cache, seg, Some(4..9)).await, b"large");
        assert_eq!(f.source.reads(), reads + 1);
    }
}
