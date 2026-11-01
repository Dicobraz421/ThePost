//! Протокол записи BLOB (SRS 3.4, CONTRACTS.md#BlobIndex): хэш → `acquire` → загрузка → `commit_new`.
//!
//! Каждый вызов [`BlobIndex`] — отдельная короткая транзакция, загрузка в [`BlobStore`] — между ними,
//! поэтому писатель не держит транзакций БД. Если запись в момент `commit_new` удаляется GC
//! ([`Commit::Bumped`]), объект загружается по новому поколению и `commit_new` повторяется.
//! Ошибка загрузки не создаёт записи: `acquire` с ответом [`Acquire::Absent`] ничего не меняет,
//! а частично загруженный объект невидим или станет «сиротой» (`orphans`).
//!
//! Ключ, по которому объект уже лежит, повторно не используется: такой объект — след оборванной
//! записи, и поиск «сирот» может удалить его по листингу, снятому до нашей загрузки, уже после
//! `commit_new`. Поэтому перед загрузкой [`BlobStore::head`]: занятый ключ — следующее поколение
//! (`commit_new` принимает поколение больше выданного). Загрузка по свободному ключу создаёт новый
//! объект с текущим моментом записи — он моложе периода ожидания и в листинг «сирот» не попадёт.
//!
//! Сжатие ([`crate::compress`]): после `acquire` с ответом [`Acquire::Absent`] объект сжимается zstd, если учёт
//! хранит признак и выигрыш достаточен; загружается сжатый вид, `commit_new_with` получает исходный размер
//! и признак. Новое поколение после `Bumped` или занятого ключа загружается в том же виде.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::{StreamExt, stream};
use schemars::JsonSchema;
use sha2::{Digest, Sha256};
use tp_contracts::blob::{Acquire, BlobIndex, BlobStore, ByteStream, Commit, Compression};
use tp_contracts::{BlobHash, BlobKey, Error, Gen, Result, TenantId};

use crate::compress::{CompressionConfig, Skip};

/// Байтов начала объекта для распознавания формата по сигнатуре.
const SNIFF_BYTES: usize = 8192;

/// Конфигурация писателя.
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
pub struct WriterConfig {
    /// Предел смен поколения на одну запись: ответы `Bumped` (запись параллельно удаляется GC)
    /// и занятые ключи; после него — `Unavailable`.
    pub max_bumps: u32,
    /// Предел размера объекта при записи из потока, байт: поток держится в памяти,
    /// пока не известен хэш.
    pub max_stream_bytes: u64,
    /// Сжатие объектов zstd.
    pub compression: CompressionConfig,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            max_bumps: 8,
            max_stream_bytes: 256 << 20,
            compression: CompressionConfig::default(),
        }
    }
}

/// Потоковый SHA-256 части письма: части подаются по мере разбора.
#[derive(Debug, Clone, Default)]
pub struct BlobHasher {
    sha: Sha256,
    len: u64,
}

impl BlobHasher {
    /// Пустой хэшер.
    pub fn new() -> Self {
        Self::default()
    }

    /// Добавляет очередную часть данных.
    pub fn update(&mut self, data: &[u8]) {
        self.sha.update(data);
        self.len += data.len() as u64;
    }

    /// Число поданных байтов.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Данных не было.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Хэш поданных данных.
    pub fn finish(self) -> BlobHash {
        BlobHash(self.sha.finalize().into())
    }

    /// Хэш данных целиком.
    pub fn digest(data: &[u8]) -> BlobHash {
        let mut h = Self::new();
        h.update(data);
        h.finish()
    }
}

/// Снимок метрик писателя (накопительные счётчики).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriterMetrics {
    /// Записей всего.
    pub writes: u64,
    /// Записей без загрузки: объект уже был (`Acquire::Existing`).
    pub dedup: u64,
    /// Загрузок объектов.
    pub uploads: u64,
    /// Загружено байтов (сжатый вид для сжатых объектов).
    pub upload_bytes: u64,
    /// Исходный размер загруженных объектов, байт: коэффициент сжатия — `upload_bytes / original_bytes`.
    pub original_bytes: u64,
    /// Загрузок в сжатом виде.
    pub compressed: u64,
    /// Загрузок без сжатия: уже сжатый формат (тип MIME или сигнатура).
    pub skipped_type: u64,
    /// Загрузок без сжатия: выигрыш меньше порога.
    pub skipped_gain: u64,
    /// Ответов `Commit::Bumped`.
    pub bumps: u64,
    /// Ключей, по которым объект уже лежал (след оборванной записи): взято следующее поколение.
    pub occupied: u64,
    /// Загрузок, оставшихся без ссылки: параллельная запись создала объект другого поколения.
    pub stale_uploads: u64,
    /// Записей, завершившихся ошибкой.
    pub errors: u64,
}

impl WriterMetrics {
    /// Коэффициент сжатия загрузок: загружено к исходному размеру (1.0 — без сжатия).
    pub fn compression_ratio(&self) -> f64 {
        if self.original_bytes == 0 {
            1.0
        } else {
            self.upload_bytes as f64 / self.original_bytes as f64
        }
    }
}

#[derive(Debug, Default)]
struct Counters {
    writes: AtomicU64,
    dedup: AtomicU64,
    uploads: AtomicU64,
    upload_bytes: AtomicU64,
    original_bytes: AtomicU64,
    compressed: AtomicU64,
    skipped_type: AtomicU64,
    skipped_gain: AtomicU64,
    bumps: AtomicU64,
    occupied: AtomicU64,
    stale_uploads: AtomicU64,
    errors: AtomicU64,
}

fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

/// Писатель объектов BLOB; ключ из ответа — для метаданных письма (шаг 4 протокола).
pub struct BlobWriter {
    index: Arc<dyn BlobIndex>,
    store: Arc<dyn BlobStore>,
    cfg: WriterConfig,
    counters: Counters,
}

impl std::fmt::Debug for BlobWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobWriter")
            .field("cfg", &self.cfg)
            .field("metrics", &self.metrics())
            .finish_non_exhaustive()
    }
}

impl BlobWriter {
    /// Писатель поверх учёта `index` и хранилища `store`.
    pub fn new(index: Arc<dyn BlobIndex>, store: Arc<dyn BlobStore>, cfg: WriterConfig) -> Self {
        Self {
            index,
            store,
            cfg,
            counters: Counters::default(),
        }
    }

    /// Снимок метрик.
    pub fn metrics(&self) -> WriterMetrics {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        WriterMetrics {
            writes: get(&c.writes),
            dedup: get(&c.dedup),
            uploads: get(&c.uploads),
            upload_bytes: get(&c.upload_bytes),
            original_bytes: get(&c.original_bytes),
            compressed: get(&c.compressed),
            skipped_type: get(&c.skipped_type),
            skipped_gain: get(&c.skipped_gain),
            bumps: get(&c.bumps),
            occupied: get(&c.occupied),
            stale_uploads: get(&c.stale_uploads),
            errors: get(&c.errors),
        }
    }

    /// Сохраняет объект `data` для `refs` получателей; хэш вычисляется здесь.
    pub async fn write(&self, tenant: TenantId, data: Bytes, refs: u32) -> Result<BlobKey> {
        let hash = BlobHasher::digest(&data);
        self.write_hashed(tenant, hash, data, refs).await
    }

    /// Сохраняет объект из потока: хэш считается по мере чтения, части держатся в памяти
    /// (без склейки) до `max_stream_bytes`.
    pub async fn write_stream(
        &self,
        tenant: TenantId,
        mut data: ByteStream,
        refs: u32,
    ) -> Result<BlobKey> {
        let mut hasher = BlobHasher::new();
        let mut chunks = Vec::new();
        while let Some(chunk) = data.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            if hasher.len() > self.cfg.max_stream_bytes {
                return Err(Error::invalid(format!(
                    "object exceeds max_stream_bytes {}",
                    self.cfg.max_stream_bytes
                )));
            }
            chunks.push(chunk);
        }
        let size = hasher.len();
        let hash = hasher.finish();
        self.run(tenant, hash, size, refs, Body::Chunks(chunks), None)
            .await
    }

    /// Сохраняет объект с уже вычисленным хэшем (хэш посчитан при разборе письма).
    pub async fn write_hashed(
        &self,
        tenant: TenantId,
        hash: BlobHash,
        data: Bytes,
        refs: u32,
    ) -> Result<BlobKey> {
        self.write_part(tenant, hash, data, refs, None).await
    }

    /// Сохраняет часть письма с уже вычисленным хэшем; `media_type` — тип MIME части: уже сжатые
    /// форматы не сжимаются (сигнатура проверяется и без него).
    pub async fn write_part(
        &self,
        tenant: TenantId,
        hash: BlobHash,
        data: Bytes,
        refs: u32,
        media_type: Option<&str>,
    ) -> Result<BlobKey> {
        let size = data.len() as u64;
        self.run(tenant, hash, size, refs, Body::Whole(data), media_type)
            .await
    }

    async fn run(
        &self,
        tenant: TenantId,
        hash: BlobHash,
        size: u64,
        refs: u32,
        body: Body,
        media_type: Option<&str>,
    ) -> Result<BlobKey> {
        add(&self.counters.writes, 1);
        let res = self
            .protocol(tenant, hash, size, refs, body, media_type)
            .await;
        if let Err(e) = &res {
            add(&self.counters.errors, 1);
            tracing::warn!(%tenant, %hash, error = %e, "запись BLOB не удалась");
        }
        res
    }

    async fn protocol(
        &self,
        tenant: TenantId,
        hash: BlobHash,
        size: u64,
        refs: u32,
        body: Body,
        media_type: Option<&str>,
    ) -> Result<BlobKey> {
        let key = |generation| BlobKey {
            tenant,
            hash,
            generation,
        };
        let mut generation = match self.index.acquire(tenant, &hash, refs).await? {
            Acquire::Existing { generation } => {
                add(&self.counters.dedup, 1);
                return Ok(key(generation));
            }
            Acquire::Absent { generation } => generation,
        };
        let (body, compression) = self.prepare(body, size, media_type).await;
        let mut bumps = 0;
        loop {
            while self.store.head(&key(generation)).await?.is_some() {
                add(&self.counters.occupied, 1);
                self.bump(&mut bumps)?;
                generation = generation.next();
            }
            self.upload(&key(generation), &body, size).await?;
            match self
                .index
                .commit_new_with(tenant, &hash, generation, size, refs, compression)
                .await?
            {
                Commit::Done { generation: done } => {
                    if done != generation {
                        add(&self.counters.stale_uploads, 1);
                    }
                    return Ok(key(done));
                }
                Commit::Bumped { generation: next } => {
                    add(&self.counters.bumps, 1);
                    self.bump(&mut bumps)?;
                    tracing::debug!(%tenant, %hash, generation = next.0, "запись BLOB удаляется GC, новое поколение");
                    generation = next_generation(generation, next);
                }
            }
        }
    }

    /// Учитывает смену поколения; сверх предела — `Unavailable`.
    fn bump(&self, bumps: &mut u32) -> Result<()> {
        *bumps += 1;
        if *bumps > self.cfg.max_bumps {
            return Err(Error::UNAVAILABLE);
        }
        Ok(())
    }

    /// Вид объекта для загрузки: сжатый, если учёт хранит признак, формат не сжат и выигрыш достаточен.
    async fn prepare(
        &self,
        body: Body,
        size: u64,
        media_type: Option<&str>,
    ) -> (Body, Compression) {
        let cfg = &self.cfg.compression;
        let skip = if self.index.supports_compression() {
            cfg.precheck(size, media_type, &body.head(SNIFF_BYTES))
        } else {
            Some(Skip::Off)
        };
        let skip = match skip {
            Some(skip) => skip,
            None => {
                let chunks = body.chunks();
                let cfg = cfg.clone();
                match tokio::task::spawn_blocking(move || cfg.compress(&chunks, size)).await {
                    Ok(Ok(zipped)) => {
                        add(&self.counters.compressed, 1);
                        return (Body::Whole(zipped), Compression::Zstd);
                    }
                    Ok(Err(skip)) => skip,
                    Err(e) => {
                        tracing::warn!(error = %e, "сжатие объекта BLOB прервано, хранится как есть");
                        Skip::Off
                    }
                }
            }
        };
        match skip {
            Skip::Type => add(&self.counters.skipped_type, 1),
            Skip::Gain => add(&self.counters.skipped_gain, 1),
            Skip::Off => {}
        }
        (body, Compression::None)
    }

    /// Загрузка вне транзакций БД; повторная загрузка того же ключа идемпотентна.
    /// `original` — размер исходного содержимого (метрики).
    async fn upload(&self, key: &BlobKey, body: &Body, original: u64) -> Result<()> {
        let size = body.len();
        match body {
            Body::Whole(data) => self.store.put(key, data.clone()).await?,
            Body::Chunks(chunks) => {
                let parts = stream::iter(chunks.clone().into_iter().map(Ok)).boxed();
                self.store.put_stream(key, parts, Some(size)).await?;
            }
        }
        add(&self.counters.uploads, 1);
        add(&self.counters.upload_bytes, size);
        add(&self.counters.original_bytes, original);
        Ok(())
    }
}

/// Поколение повторной загрузки: выданное учётом, но не меньше следующего за текущим
/// (защита от зацикливания на неверном ответе).
fn next_generation(current: Gen, bumped: Gen) -> Gen {
    bumped.max(current.next())
}

/// Тело объекта: целиком или частями потока.
enum Body {
    Whole(Bytes),
    Chunks(Vec<Bytes>),
}

impl Body {
    fn len(&self) -> u64 {
        match self {
            Body::Whole(data) => data.len() as u64,
            Body::Chunks(chunks) => chunks.iter().map(|c| c.len() as u64).sum(),
        }
    }

    /// Части тела (без копирования байтов).
    fn chunks(&self) -> Vec<Bytes> {
        match self {
            Body::Whole(data) => vec![data.clone()],
            Body::Chunks(chunks) => chunks.clone(),
        }
    }

    /// Начало тела, не длиннее `n` байт.
    fn head(&self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        for c in self.chunks() {
            let take = (n - out.len()).min(c.len());
            out.extend_from_slice(&c[..take]);
            if out.len() == n {
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use time::OffsetDateTime;
    use tp_testkit::blob::{BlobOp, BlobState, MemBlobIndex, MemBlobStore, read_all};

    const T: TenantId = TenantId::DEFAULT;

    fn setup() -> (MemBlobIndex, MemBlobStore, BlobWriter) {
        let index = MemBlobIndex::default();
        let store = MemBlobStore::default();
        let writer = BlobWriter::new(
            Arc::new(index.clone()),
            Arc::new(store.clone()),
            WriterConfig::default(),
        );
        (index, store, writer)
    }

    #[test]
    fn hasher_is_sha256() {
        // SHA-256("abc") — тестовый вектор FIPS 180-2.
        let h = BlobHasher::digest(b"abc");
        assert_eq!(
            h.to_string(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut parts = BlobHasher::new();
        parts.update(b"a");
        parts.update(b"bc");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.finish(), h);
    }

    #[tokio::test]
    async fn dedup() {
        let (index, store, writer) = setup();
        let data = Bytes::from_static(b"body");
        let a = writer.write(T, data.clone(), 2).await.expect("вызов");
        let b = writer.write(T, data.clone(), 3).await.expect("вызов");
        assert_eq!(a, b);
        assert_eq!(store.calls(BlobOp::Put), 1);
        let row = index.row(T, &a.hash).expect("значение");
        assert_eq!((row.refcnt, row.state, row.size), (5, BlobState::Live, 4));
        assert_eq!(
            read_all(store.get(&a, None).await.expect("вызов"))
                .await
                .expect("вызов"),
            data
        );
        let m = writer.metrics();
        assert_eq!((m.writes, m.dedup, m.uploads, m.upload_bytes), (2, 1, 1, 4));
    }

    #[tokio::test]
    async fn stream_matches_whole() {
        let (index, store, writer) = setup();
        let parts = stream::iter([
            Ok(Bytes::from_static(b"he")),
            Ok(Bytes::from_static(b"llo")),
        ]);
        let key = writer
            .write_stream(T, parts.boxed(), 1)
            .await
            .expect("вызов");
        assert_eq!(key.hash, BlobHasher::digest(b"hello"));
        assert_eq!(
            store.bytes(&key.path()).expect("значение"),
            Bytes::from_static(b"hello")
        );
        assert_eq!(index.row(T, &key.hash).expect("значение").size, 5);
        // Тот же объект целиком — дедупликация.
        writer
            .write(T, Bytes::from_static(b"hello"), 1)
            .await
            .expect("вызов");
        assert_eq!(writer.metrics().dedup, 1);
    }

    #[tokio::test]
    async fn stream_limit() {
        let index = MemBlobIndex::default();
        let store = MemBlobStore::default();
        let cfg = WriterConfig {
            max_stream_bytes: 3,
            ..WriterConfig::default()
        };
        let writer = BlobWriter::new(Arc::new(index.clone()), Arc::new(store.clone()), cfg);
        let parts = stream::iter([Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"cd"))]);
        assert!(matches!(
            writer.write_stream(T, parts.boxed(), 1).await,
            Err(Error::Invalid(_))
        ));
        assert!(store.paths().is_empty());
    }

    #[tokio::test]
    async fn upload_error_creates_no_record() {
        let (index, store, writer) = setup();
        store.fail_next(BlobOp::Put, Error::UNAVAILABLE);
        let data = Bytes::from_static(b"x");
        let hash = BlobHasher::digest(&data);
        assert_eq!(
            writer.write(T, data.clone(), 1).await,
            Err(Error::UNAVAILABLE)
        );
        assert_eq!(index.row(T, &hash), None);
        assert!(store.paths().is_empty());
        assert_eq!(writer.metrics().errors, 1);
        // Повтор после сбоя — обычная запись первого поколения.
        let key = writer.write(T, data, 1).await.expect("вызов");
        assert_eq!(key.generation, Gen::INITIAL);
    }

    #[tokio::test]
    async fn deleting_record_gets_next_generation() {
        let (index, store, writer) = setup();
        let data = Bytes::from_static(b"gc");
        let hash = BlobHasher::digest(&data);
        // Запись поколения 0 обнулена и переведена GC в `deleting`.
        let old = writer.write(T, data.clone(), 1).await.expect("вызов");
        index
            .release(T, &hash, old.generation, 1)
            .await
            .expect("вызов");
        let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
        assert!(index.mark_deleting(&old, later).await.expect("вызов"));

        let key = writer.write(T, data.clone(), 1).await.expect("вызов");
        assert_eq!(key.generation, Gen(1));
        assert!(store.contains(&key));
        let row = index.row(T, &hash).expect("значение");
        assert_eq!(
            (row.generation, row.state, row.refcnt),
            (Gen(1), BlobState::Live, 1)
        );
        // GC не может завершить удаление: запись уже другого поколения.
        assert!(!index.finalize_delete(&old).await.expect("вызов"));
    }

    #[tokio::test]
    async fn bumped_during_upload() {
        let (index, store, writer) = setup();
        let writer = Arc::new(writer);
        let data = Bytes::from_static(b"race");
        let hash = BlobHasher::digest(&data);
        let old = BlobKey {
            tenant: T,
            hash,
            generation: Gen::INITIAL,
        };
        // Писатель получил `Absent` и загружает поколение 0 медленно.
        store.set_delay(BlobOp::Put, std::time::Duration::from_millis(100));
        let task = tokio::spawn({
            let writer = Arc::clone(&writer);
            let data = data.clone();
            async move { writer.write(T, data, 1).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Тем временем другая запись создала поколение 0, ссылка снята, GC начал удаление.
        assert_eq!(
            index
                .commit_new(T, &hash, Gen::INITIAL, 4, 1)
                .await
                .expect("вызов"),
            Commit::Done {
                generation: Gen::INITIAL
            }
        );
        index
            .release(T, &hash, Gen::INITIAL, 1)
            .await
            .expect("вызов");
        let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
        assert!(index.mark_deleting(&old, later).await.expect("вызов"));

        let key = task.await.expect("вызов").expect("значение");
        assert_eq!(key.generation, Gen(1));
        assert_eq!(writer.metrics().bumps, 1);
        assert_eq!(store.calls(BlobOp::Put), 2);
        // GC удаляет объект поколения 0; запись уже нового поколения и надгробием не заменяется.
        store.delete(&old).await.expect("вызов");
        assert!(!index.finalize_delete(&old).await.expect("вызов"));
        assert!(store.contains(&key));
        assert_eq!(
            index.row(T, &hash).expect("значение").state,
            BlobState::Live
        );
    }

    #[tokio::test]
    async fn occupied_key_skipped() {
        let (index, store, writer) = setup();
        let data = Bytes::from_static(b"left");
        let hash = BlobHasher::digest(&data);
        // След оборванной записи: объект поколения 0 без записи учёта.
        let left = BlobKey {
            tenant: T,
            hash,
            generation: Gen::INITIAL,
        };
        store.put(&left, data.clone()).await.expect("вызов");
        let key = writer.write(T, data, 1).await.expect("вызов");
        assert_eq!(key.generation, Gen(1));
        assert!(!index.exists(&left).await.expect("вызов"));
        assert!(index.exists(&key).await.expect("вызов"));
        assert_eq!(writer.metrics().occupied, 1);
    }

    /// Учёт, который на первые `n` вызовов `commit_new` отвечает `Bumped`.
    struct Bumping {
        inner: MemBlobIndex,
        left: std::sync::Mutex<u32>,
    }

    #[async_trait]
    impl BlobIndex for Bumping {
        async fn acquire(&self, t: TenantId, h: &BlobHash, refs: u32) -> Result<Acquire> {
            self.inner.acquire(t, h, refs).await
        }
        async fn commit_new(
            &self,
            t: TenantId,
            h: &BlobHash,
            generation: Gen,
            size: u64,
            refs: u32,
        ) -> Result<Commit> {
            {
                let mut left = self.left.lock().expect("lock");
                if *left > 0 {
                    *left -= 1;
                    return Ok(Commit::Bumped {
                        generation: generation.next(),
                    });
                }
            }
            self.inner.commit_new(t, h, generation, size, refs).await
        }
        async fn release(&self, t: TenantId, h: &BlobHash, g: Gen, refs: u32) -> Result<()> {
            self.inner.release(t, h, g, refs).await
        }
        async fn gc_candidates(&self, o: OffsetDateTime, l: usize) -> Result<Vec<BlobKey>> {
            self.inner.gc_candidates(o, l).await
        }
        async fn mark_deleting(&self, k: &BlobKey, o: OffsetDateTime) -> Result<bool> {
            self.inner.mark_deleting(k, o).await
        }
        async fn finalize_delete(&self, k: &BlobKey) -> Result<bool> {
            self.inner.finalize_delete(k).await
        }
        async fn exists(&self, k: &BlobKey) -> Result<bool> {
            self.inner.exists(k).await
        }
    }

    #[tokio::test]
    async fn bumps_are_bounded() {
        let store = MemBlobStore::default();
        let cfg = WriterConfig {
            max_bumps: 2,
            ..WriterConfig::default()
        };
        let index = Arc::new(Bumping {
            inner: MemBlobIndex::default(),
            left: std::sync::Mutex::new(2),
        });
        let writer = BlobWriter::new(index.clone(), Arc::new(store.clone()), cfg.clone());
        let key = writer
            .write(T, Bytes::from_static(b"b"), 1)
            .await
            .expect("вызов");
        assert_eq!(key.generation, Gen(2));
        assert_eq!(store.calls(BlobOp::Put), 3);
        assert_eq!(writer.metrics().bumps, 2);

        *index.left.lock().expect("lock") = 3;
        let writer = BlobWriter::new(index, Arc::new(store), cfg);
        assert_eq!(
            writer.write(T, Bytes::from_static(b"c"), 1).await,
            Err(Error::UNAVAILABLE)
        );
    }
}
