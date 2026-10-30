//! In-memory хранилище объектов и учёт объектов (CONTRACTS.md#BlobStore, #BlobIndex; SRS 3.4).
//!
//! [`MemBlobStore`] повторяет поведение `tp-blob` (идемпотентная запись, обрезка диапазона, проверка
//! произвольных путей) и умеет инъекцию ошибок и задержек по операциям ([`BlobOp`]).
//! [`MemBlobIndex`] — протокол поколений `tp-meta`: состояния `live` → `deleting` → надгробие `deleted`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::{StreamExt, TryStreamExt, stream};
use time::OffsetDateTime;
use tp_contracts::blob::{
    Acquire, BlobIndex, BlobStore, ByteStream, Commit, ObjectEntry, ObjectStream,
};
use tp_contracts::{BlobHash, BlobKey, Error, Gen, Result, TenantId};

use crate::infra::{Clock, SystemClock, lock};

/// Операция [`MemBlobStore`] для инъекции ошибок и задержек.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlobOp {
    /// `put`, `put_stream`.
    Put,
    /// `get`.
    Get,
    /// `head`.
    Head,
    /// `delete`.
    Delete,
    /// `list`.
    List,
    /// `put_raw`.
    PutRaw,
    /// `get_raw`.
    GetRaw,
    /// `delete_raw`.
    DeleteRaw,
}

#[derive(Debug, Clone)]
struct Object {
    data: Bytes,
    modified: OffsetDateTime,
}

#[derive(Debug, Default)]
struct Faults {
    errors: HashMap<BlobOp, VecDeque<Error>>,
    delays: HashMap<BlobOp, Duration>,
    calls: HashMap<BlobOp, u64>,
}

/// Хранилище объектов в памяти; клоны разделяют данные.
#[derive(Debug, Clone)]
pub struct MemBlobStore {
    objects: Arc<Mutex<BTreeMap<String, Object>>>,
    faults: Arc<Mutex<Faults>>,
    clock: Arc<dyn Clock>,
    chunk_size: usize,
}

impl MemBlobStore {
    /// Хранилище с часами `clock` (момент записи объектов для `list`).
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            objects: Arc::default(),
            faults: Arc::default(),
            clock,
            chunk_size: 64 * 1024,
        }
    }

    /// Размер кусков потока `get` (по умолчанию 64 КиБ), не меньше 1 байта.
    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size.max(1);
        self
    }

    /// Следующий вызов `op` вернёт `err` (ошибки копятся в очередь).
    pub fn fail_next(&self, op: BlobOp, err: Error) {
        lock(&self.faults)
            .errors
            .entry(op)
            .or_default()
            .push_back(err);
    }

    /// Следующие `n` вызовов `op` вернут `err`.
    pub fn fail_times(&self, op: BlobOp, n: usize, err: Error) {
        for _ in 0..n {
            self.fail_next(op, err.clone());
        }
    }

    /// Каждый вызов `op` ждёт `delay` перед выполнением (`Duration::ZERO` — без задержки).
    pub fn set_delay(&self, op: BlobOp, delay: Duration) {
        lock(&self.faults).delays.insert(op, delay);
    }

    /// Снимает все ошибки и задержки.
    pub fn clear_faults(&self) {
        let mut f = lock(&self.faults);
        f.errors.clear();
        f.delays.clear();
    }

    /// Число вызовов `op` (включая завершившиеся ошибкой).
    pub fn calls(&self, op: BlobOp) -> u64 {
        lock(&self.faults).calls.get(&op).copied().unwrap_or(0)
    }

    /// Пути всех объектов по возрастанию.
    pub fn paths(&self) -> Vec<String> {
        lock(&self.objects).keys().cloned().collect()
    }

    /// Есть ли объект `key`.
    pub fn contains(&self, key: &BlobKey) -> bool {
        lock(&self.objects).contains_key(&key.path())
    }

    /// Содержимое объекта по пути.
    pub fn bytes(&self, path: &str) -> Option<Bytes> {
        lock(&self.objects).get(path).map(|o| o.data.clone())
    }

    /// Меняет момент записи объекта (тесты поиска «сирот»); `false` — объекта нет.
    pub fn set_modified(&self, path: &str, modified: OffsetDateTime) -> bool {
        lock(&self.objects)
            .get_mut(path)
            .map(|o| o.modified = modified)
            .is_some()
    }

    /// Учитывает вызов, выдерживает задержку и возвращает назначенную ошибку.
    async fn enter(&self, op: BlobOp) -> Result<()> {
        let (delay, err) = {
            let mut f = lock(&self.faults);
            *f.calls.entry(op).or_default() += 1;
            let err = f.errors.get_mut(&op).and_then(VecDeque::pop_front);
            (f.delays.get(&op).copied(), err)
        };
        if let Some(d) = delay.filter(|d| !d.is_zero()) {
            tokio::time::sleep(d).await;
        }
        err.map_or(Ok(()), Err)
    }

    fn store(&self, path: String, data: Bytes) {
        let modified = self.clock.now();
        // Объекты неизменяемы: повторная запись не меняет ни данные, ни момент записи.
        lock(&self.objects)
            .entry(path)
            .or_insert(Object { data, modified });
    }

    fn read(&self, path: &str, range: Option<Range<u64>>) -> Result<ByteStream> {
        let data = lock(&self.objects)
            .get(path)
            .map(|o| o.data.clone())
            .ok_or(Error::NotFound)?;
        let range = clamp_range(range, data.len() as u64)?;
        // Диапазон обрезан по длине объекта, поэтому помещается в usize.
        let mut data = data.slice(range.start as usize..range.end as usize);
        let mut chunks = Vec::new();
        while !data.is_empty() {
            chunks.push(Ok(data.split_to(self.chunk_size.min(data.len()))));
        }
        Ok(stream::iter(chunks).boxed())
    }
}

impl Default for MemBlobStore {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

/// Диапазон чтения объекта размером `size` (как в `tp-blob`): конец обрезается по размеру,
/// начало за концом объекта или после конца диапазона — `Invalid`.
fn clamp_range(range: Option<Range<u64>>, size: u64) -> Result<Range<u64>> {
    let Some(range) = range else {
        return Ok(0..size);
    };
    if range.start > range.end || range.start > size {
        return Err(Error::invalid(format!(
            "range {range:?} out of object size {size}"
        )));
    }
    Ok(range.start..range.end.min(size))
}

/// Произвольный путь (как в `tp-blob`): относительный, без пустых сегментов и сегментов с `.` в начале.
fn check_raw_path(path: &str) -> Result<()> {
    let bad = path.is_empty()
        || path.contains(['\\', '\0'])
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment.starts_with('.'));
    if bad {
        return Err(Error::invalid(format!("bad object path: {path}")));
    }
    Ok(())
}

#[async_trait]
impl BlobStore for MemBlobStore {
    async fn put(&self, key: &BlobKey, data: Bytes) -> Result<()> {
        self.enter(BlobOp::Put).await?;
        self.store(key.path(), data);
        Ok(())
    }

    async fn put_stream(
        &self,
        key: &BlobKey,
        data: ByteStream,
        size_hint: Option<u64>,
    ) -> Result<()> {
        self.enter(BlobOp::Put).await?;
        let cap = size_hint
            .and_then(|s| usize::try_from(s).ok())
            .unwrap_or(0)
            .min(1 << 20);
        // Ошибка потока — объект не появляется.
        let buf = data
            .try_fold(BytesMut::with_capacity(cap), |mut buf, chunk| async move {
                buf.extend_from_slice(&chunk);
                Ok(buf)
            })
            .await?;
        self.store(key.path(), buf.freeze());
        Ok(())
    }

    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream> {
        self.enter(BlobOp::Get).await?;
        self.read(&key.path(), range)
    }

    async fn head(&self, key: &BlobKey) -> Result<Option<u64>> {
        self.enter(BlobOp::Head).await?;
        Ok(lock(&self.objects)
            .get(&key.path())
            .map(|o| o.data.len() as u64))
    }

    async fn delete(&self, key: &BlobKey) -> Result<()> {
        self.enter(BlobOp::Delete).await?;
        lock(&self.objects).remove(&key.path());
        Ok(())
    }

    fn list(&self, prefix: &str, older_than: OffsetDateTime) -> ObjectStream {
        // Синхронный метод: ошибка инъекции — первым элементом потока, задержка не применяется.
        let err = {
            let mut f = lock(&self.faults);
            *f.calls.entry(BlobOp::List).or_default() += 1;
            f.errors
                .get_mut(&BlobOp::List)
                .and_then(VecDeque::pop_front)
        };
        if let Some(e) = err {
            return stream::iter([Err(e)]).boxed();
        }
        let entries: Vec<_> = lock(&self.objects)
            .range(prefix.to_owned()..)
            .take_while(|(path, _)| path.starts_with(prefix))
            .filter(|(_, o)| o.modified < older_than)
            .map(|(path, o)| {
                Ok(ObjectEntry {
                    path: path.clone(),
                    size: o.data.len() as u64,
                    modified: o.modified,
                })
            })
            .collect();
        stream::iter(entries).boxed()
    }

    async fn put_raw(&self, path: &str, data: Bytes) -> Result<()> {
        self.enter(BlobOp::PutRaw).await?;
        check_raw_path(path)?;
        // Сегменты индексов перезаписываются (в отличие от объектов BLOB).
        let modified = self.clock.now();
        lock(&self.objects).insert(path.to_owned(), Object { data, modified });
        Ok(())
    }

    async fn get_raw(&self, path: &str, range: Option<Range<u64>>) -> Result<ByteStream> {
        self.enter(BlobOp::GetRaw).await?;
        check_raw_path(path)?;
        self.read(path, range)
    }

    async fn delete_raw(&self, path: &str) -> Result<()> {
        self.enter(BlobOp::DeleteRaw).await?;
        check_raw_path(path)?;
        lock(&self.objects).remove(path);
        Ok(())
    }
}

/// Читает поток объекта целиком (хелпер тестов).
pub async fn read_all(stream: ByteStream) -> Result<Bytes> {
    let buf = stream
        .try_fold(BytesMut::new(), |mut buf, chunk| async move {
            buf.extend_from_slice(&chunk);
            Ok(buf)
        })
        .await?;
    Ok(buf.freeze())
}

// ---------- Учёт объектов ----------

/// Состояние записи учёта объекта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobState {
    /// Объект доступен для ссылок.
    Live,
    /// GC удаляет объект этого поколения.
    Deleting,
    /// Надгробие: объект поколения удалён; следующая загрузка — со следующим поколением.
    Deleted,
}

/// Запись учёта объекта (для проверок в тестах).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobRow {
    /// Поколение.
    pub generation: Gen,
    /// Счётчик ссылок.
    pub refcnt: u32,
    /// Состояние.
    pub state: BlobState,
    /// Размер объекта.
    pub size: u64,
    /// Момент обнуления счётчика.
    pub zero_since: Option<OffsetDateTime>,
}

/// Учёт объектов в памяти; клоны разделяют данные. Каждый метод атомарен (одна блокировка),
/// как одна строка `blobs` в `tp-meta`. Надгробия хранятся до [`MemBlobIndex::prune_tombstones`].
#[derive(Debug, Clone)]
pub struct MemBlobIndex {
    rows: Arc<Mutex<HashMap<(TenantId, BlobHash), BlobRow>>>,
    clock: Arc<dyn Clock>,
}

impl MemBlobIndex {
    /// Учёт с часами `clock` (момент обнуления счётчика).
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            rows: Arc::default(),
            clock,
        }
    }

    /// Запись учёта объекта.
    pub fn row(&self, tenant: TenantId, hash: &BlobHash) -> Option<BlobRow> {
        lock(&self.rows).get(&(tenant, *hash)).copied()
    }

    /// Изменяет счётчики записей `live` с совпадающим поколением на `delta` за каждое вхождение ключа
    /// (учёт ссылок писем в `MemMetadata`, как `adjust_refs` в `tp-meta`).
    pub(crate) fn adjust(&self, keys: &[BlobKey], delta: i64) {
        let now = self.clock.now();
        let mut rows = lock(&self.rows);
        for k in keys {
            if let Some(r) = rows
                .get_mut(&(k.tenant, k.hash))
                .filter(|r| r.generation == k.generation && r.state == BlobState::Live)
            {
                let n = (i64::from(r.refcnt) + delta).clamp(0, i64::from(u32::MAX));
                r.refcnt = u32::try_from(n).unwrap_or(u32::MAX);
                r.zero_since = (r.refcnt == 0).then_some(now);
            }
        }
    }

    /// Удаляет надгробия, обнулённые раньше `older_than`.
    pub fn prune_tombstones(&self, older_than: OffsetDateTime) {
        lock(&self.rows).retain(|_, r| {
            r.state != BlobState::Deleted || r.zero_since.is_none_or(|z| z >= older_than)
        });
    }
}

impl Default for MemBlobIndex {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

/// Число ссылок операции: не меньше одной (как в `tp-meta`).
fn refs_arg(refs: u32) -> Result<u32> {
    if refs == 0 {
        return Err(Error::invalid("число ссылок должно быть больше нуля"));
    }
    Ok(refs)
}

#[async_trait]
impl BlobIndex for MemBlobIndex {
    async fn acquire(&self, tenant: TenantId, hash: &BlobHash, refs: u32) -> Result<Acquire> {
        let refs = refs_arg(refs)?;
        let mut rows = lock(&self.rows);
        Ok(match rows.get_mut(&(tenant, *hash)) {
            Some(r) if r.state == BlobState::Live => {
                r.refcnt = r.refcnt.checked_add(refs).ok_or(Error::Conflict)?;
                r.zero_since = None;
                Acquire::Existing {
                    generation: r.generation,
                }
            }
            // Запись `deleting` или надгробие: объект этого поколения удаляется или удалён.
            Some(r) => Acquire::Absent {
                generation: r.generation.next(),
            },
            None => Acquire::Absent {
                generation: Gen::INITIAL,
            },
        })
    }

    async fn commit_new(
        &self,
        tenant: TenantId,
        hash: &BlobHash,
        generation: Gen,
        size: u64,
        refs: u32,
    ) -> Result<Commit> {
        let refs = refs_arg(refs)?;
        let live = BlobRow {
            generation,
            refcnt: refs,
            state: BlobState::Live,
            size,
            zero_since: None,
        };
        let mut rows = lock(&self.rows);
        let Some(r) = rows.get_mut(&(tenant, *hash)) else {
            rows.insert((tenant, *hash), live);
            return Ok(Commit::Done { generation });
        };
        Ok(if r.state == BlobState::Live {
            // Параллельная загрузка уже создала запись: ссылаться на её поколение.
            r.refcnt = r.refcnt.checked_add(refs).ok_or(Error::Conflict)?;
            r.zero_since = None;
            Commit::Done {
                generation: r.generation,
            }
        } else if r.generation < generation {
            *r = live;
            Commit::Done { generation }
        } else {
            Commit::Bumped {
                generation: r.generation.next(),
            }
        })
    }

    async fn release(
        &self,
        tenant: TenantId,
        hash: &BlobHash,
        generation: Gen,
        refs: u32,
    ) -> Result<()> {
        let refs = refs_arg(refs)?;
        let now = self.clock.now();
        let mut rows = lock(&self.rows);
        let r = rows
            .get_mut(&(tenant, *hash))
            .filter(|r| r.generation == generation && r.state == BlobState::Live)
            .ok_or(Error::NotFound)?;
        r.refcnt = r.refcnt.checked_sub(refs).ok_or(Error::Conflict)?;
        if r.refcnt == 0 {
            r.zero_since = Some(now);
        }
        Ok(())
    }

    async fn gc_candidates(
        &self,
        older_than: OffsetDateTime,
        limit: usize,
    ) -> Result<Vec<BlobKey>> {
        let rows = lock(&self.rows);
        let mut found: Vec<_> = rows
            .iter()
            .filter(|(_, r)| r.refcnt == 0 && r.state != BlobState::Deleted)
            .filter_map(|(&(tenant, hash), r)| {
                r.zero_since.filter(|z| *z < older_than).map(|z| {
                    (
                        z,
                        BlobKey {
                            tenant,
                            hash,
                            generation: r.generation,
                        },
                    )
                })
            })
            .collect();
        found.sort();
        Ok(found.into_iter().take(limit).map(|(_, k)| k).collect())
    }

    async fn mark_deleting(&self, key: &BlobKey, older_than: OffsetDateTime) -> Result<bool> {
        let mut rows = lock(&self.rows);
        let Some(r) = rows.get_mut(&(key.tenant, key.hash)).filter(|r| {
            r.generation == key.generation
                && r.refcnt == 0
                && r.state != BlobState::Deleted
                && r.zero_since.is_some_and(|z| z < older_than)
        }) else {
            return Ok(false);
        };
        r.state = BlobState::Deleting;
        Ok(true)
    }

    async fn finalize_delete(&self, key: &BlobKey) -> Result<bool> {
        let now = self.clock.now();
        let mut rows = lock(&self.rows);
        let Some(r) = rows
            .get_mut(&(key.tenant, key.hash))
            .filter(|r| r.generation == key.generation && r.state == BlobState::Deleting)
        else {
            return Ok(false);
        };
        r.state = BlobState::Deleted;
        r.zero_since = Some(now);
        Ok(true)
    }

    async fn exists(&self, key: &BlobKey) -> Result<bool> {
        Ok(lock(&self.rows)
            .get(&(key.tenant, key.hash))
            .is_some_and(|r| r.generation == key.generation && r.state != BlobState::Deleted))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::ManualClock;

    const W: Duration = Duration::from_secs(3600);

    fn hash(n: u8) -> BlobHash {
        BlobHash([n; 32])
    }

    fn key(n: u8, generation: Gen) -> BlobKey {
        BlobKey {
            tenant: TenantId::DEFAULT,
            hash: hash(n),
            generation,
        }
    }

    fn setup() -> (ManualClock, MemBlobIndex) {
        let clock = ManualClock::default();
        let index = MemBlobIndex::new(clock.shared());
        (clock, index)
    }

    #[tokio::test]
    async fn acquire_commit_release() {
        let (clock, idx) = setup();
        let t = TenantId::DEFAULT;
        let h = hash(1);
        assert_eq!(
            idx.acquire(t, &h, 3).await.expect("ok"),
            Acquire::Absent {
                generation: Gen::INITIAL
            }
        );
        assert_eq!(
            idx.commit_new(t, &h, Gen::INITIAL, 10, 3)
                .await
                .expect("ok"),
            Commit::Done {
                generation: Gen::INITIAL
            }
        );
        assert_eq!(
            idx.acquire(t, &h, 2).await.expect("ok"),
            Acquire::Existing {
                generation: Gen::INITIAL
            }
        );
        assert_eq!(idx.row(t, &h).expect("ok").refcnt, 5);

        // Параллельная загрузка того же объекта ссылается на уже созданную запись.
        assert_eq!(
            idx.commit_new(t, &h, Gen::INITIAL, 10, 1)
                .await
                .expect("ok"),
            Commit::Done {
                generation: Gen::INITIAL
            }
        );
        idx.release(t, &h, Gen::INITIAL, 4).await.expect("ok");
        assert_eq!(idx.row(t, &h).expect("ok").zero_since, None);
        idx.release(t, &h, Gen::INITIAL, 2).await.expect("ok");
        assert_eq!(idx.row(t, &h).expect("ok").zero_since, Some(clock.now()));
        assert_eq!(
            idx.release(t, &h, Gen::INITIAL, 1).await,
            Err(Error::Conflict)
        );
        assert_eq!(idx.release(t, &h, Gen(7), 1).await, Err(Error::NotFound));
        assert!(idx.acquire(t, &h, 0).await.is_err());

        // Новая ссылка снимает обнуление: GC не трогает объект.
        idx.acquire(t, &h, 1).await.expect("ok");
        clock.advance(W * 2);
        assert!(
            idx.gc_candidates(clock.now() - W, 10)
                .await
                .expect("ok")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn gc_window_and_bump() {
        let (clock, idx) = setup();
        let t = TenantId::DEFAULT;
        let h = hash(2);
        let k0 = key(2, Gen::INITIAL);
        idx.acquire(t, &h, 1).await.expect("ok");
        idx.commit_new(t, &h, Gen::INITIAL, 5, 1).await.expect("ok");
        idx.release(t, &h, Gen::INITIAL, 1).await.expect("ok");

        // Окно W не истекло.
        assert!(!idx.mark_deleting(&k0, clock.now() - W).await.expect("ok"));
        clock.advance(W + Duration::from_secs(1));
        let older = clock.now() - W;
        assert_eq!(idx.gc_candidates(older, 10).await.expect("ok"), [k0]);
        assert!(idx.mark_deleting(&k0, older).await.expect("ok"));
        assert!(idx.exists(&k0).await.expect("ok"));

        // Запись deleting: новое письмо загружает следующее поколение.
        let Acquire::Absent { generation: g1 } = idx.acquire(t, &h, 2).await.expect("ok") else {
            panic!("ожидалось Absent");
        };
        assert_eq!(g1, Gen(1));
        // Загрузка по старому поколению (начата до GC) — Bumped.
        assert_eq!(
            idx.commit_new(t, &h, Gen::INITIAL, 5, 2).await.expect("ok"),
            Commit::Bumped { generation: g1 }
        );
        assert_eq!(
            idx.commit_new(t, &h, g1, 5, 2).await.expect("ok"),
            Commit::Done { generation: g1 }
        );

        // Поколение изменилось: GC не удаляет запись нового поколения.
        assert!(!idx.finalize_delete(&k0).await.expect("ok"));
        assert!(!idx.exists(&k0).await.expect("ok"));
        assert!(idx.exists(&key(2, g1)).await.expect("ok"));
        assert_eq!(idx.row(t, &h).expect("ok").state, BlobState::Live);
    }

    #[tokio::test]
    async fn finalize_only_same_gen_and_tombstone() {
        let (clock, idx) = setup();
        let t = TenantId::DEFAULT;
        let h = hash(3);
        let k0 = key(3, Gen::INITIAL);
        idx.commit_new(t, &h, Gen::INITIAL, 1, 1).await.expect("ok");
        idx.release(t, &h, Gen::INITIAL, 1).await.expect("ok");
        clock.advance(W * 2);
        let older = clock.now() - W;
        assert!(!idx.finalize_delete(&k0).await.expect("ok"), "не deleting");
        assert!(idx.mark_deleting(&k0, older).await.expect("ok"));
        assert!(!idx.finalize_delete(&key(3, Gen(1))).await.expect("ok"));
        assert!(idx.finalize_delete(&k0).await.expect("ok"));
        assert!(!idx.exists(&k0).await.expect("ok"));
        assert!(idx.gc_candidates(older, 10).await.expect("ok").is_empty());

        // Загрузка, начатая до удаления по удалённому поколению, получает Bumped.
        assert_eq!(
            idx.commit_new(t, &h, Gen::INITIAL, 1, 1).await.expect("ok"),
            Commit::Bumped { generation: Gen(1) }
        );
        assert_eq!(
            idx.acquire(t, &h, 1).await.expect("ok"),
            Acquire::Absent { generation: Gen(1) }
        );
        idx.prune_tombstones(clock.now() + W);
        assert_eq!(idx.row(t, &h), None);
    }

    #[tokio::test]
    async fn store_roundtrip_ranges_list() {
        let clock = ManualClock::default();
        let store = MemBlobStore::new(clock.shared()).with_chunk_size(3);
        let k = key(4, Gen::INITIAL);
        store
            .put(&k, Bytes::from_static(b"hello world"))
            .await
            .expect("ok");
        store
            .put(&k, Bytes::from_static(b"other"))
            .await
            .expect("ok");
        assert_eq!(
            read_all(store.get(&k, None).await.expect("ok"))
                .await
                .expect("ok"),
            "hello world"
        );
        assert_eq!(store.get(&k, None).await.expect("ok").count().await, 4);
        assert_eq!(
            read_all(store.get(&k, Some(6..100)).await.expect("ok"))
                .await
                .expect("ok"),
            "world"
        );
        assert!(store.get(&k, Some(12..13)).await.is_err());
        assert_eq!(store.head(&k).await.expect("ok"), Some(11));
        assert!(matches!(
            store.get(&key(5, Gen::INITIAL), None).await,
            Err(Error::NotFound)
        ));

        store
            .put_raw("idx/t/a/seg1", Bytes::from_static(b"s1"))
            .await
            .expect("ok");
        store
            .put_raw("idx/t/a/seg1", Bytes::from_static(b"s2"))
            .await
            .expect("ok");
        assert_eq!(
            read_all(store.get_raw("idx/t/a/seg1", None).await.expect("ok"))
                .await
                .expect("ok"),
            "s2"
        );
        assert!(store.put_raw("../x", Bytes::new()).await.is_err());

        clock.advance(W);
        let all: Vec<_> = store.list("", clock.now()).try_collect().await.expect("ok");
        assert_eq!(all.len(), 2);
        let blobs: Vec<_> = store
            .list(&BlobKey::tenant_prefix(TenantId::DEFAULT), clock.now())
            .try_collect()
            .await
            .expect("ok");
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].key(), Some(k));
        assert!(store.set_modified(&k.path(), clock.now()));
        let old: Vec<_> = store.list("", clock.now()).try_collect().await.expect("ok");
        assert_eq!(old.len(), 1);

        store.delete(&k).await.expect("ok");
        store.delete(&k).await.expect("ok");
        store.delete_raw("idx/t/a/seg1").await.expect("ok");
        assert!(store.paths().is_empty());
    }

    #[tokio::test]
    async fn store_faults_and_failed_stream() {
        let store = MemBlobStore::default();
        let k = key(6, Gen::INITIAL);
        store.fail_times(BlobOp::Put, 2, Error::UNAVAILABLE);
        assert!(matches!(
            store.put(&k, Bytes::from_static(b"x")).await,
            Err(e) if e.is_retryable()
        ));
        assert!(store.put(&k, Bytes::from_static(b"x")).await.is_err());
        store.put(&k, Bytes::from_static(b"x")).await.expect("ok");
        assert_eq!(store.calls(BlobOp::Put), 3);

        let broken: ByteStream =
            stream::iter([Ok(Bytes::from_static(b"part")), Err(Error::UNAVAILABLE)]).boxed();
        let k2 = key(7, Gen::INITIAL);
        assert!(store.put_stream(&k2, broken, Some(8)).await.is_err());
        assert!(!store.contains(&k2));

        store.fail_next(BlobOp::List, Error::internal("boom"));
        let listed: Result<Vec<_>> = store
            .list("", OffsetDateTime::now_utc())
            .try_collect()
            .await;
        assert!(listed.is_err());

        store.set_delay(BlobOp::Head, Duration::from_millis(20));
        let started = std::time::Instant::now();
        assert_eq!(store.head(&k).await.expect("ok"), Some(1));
        assert!(started.elapsed() >= Duration::from_millis(20));
        store.clear_faults();
        store.fail_next(BlobOp::Get, Error::NotFound);
        store.clear_faults();
        assert!(store.get(&k, None).await.is_ok());
    }

    /// Полный цикл по протоколу контракта: запись, освобождение, GC, «сироты».
    #[tokio::test]
    async fn protocol_end_to_end() {
        let clock = ManualClock::default();
        let idx = MemBlobIndex::new(clock.shared());
        let store = MemBlobStore::new(clock.shared());
        let t = TenantId::DEFAULT;
        let h = hash(8);
        let data = Bytes::from_static(b"attachment");

        let Acquire::Absent { generation } = idx.acquire(t, &h, 2).await.expect("ok") else {
            panic!("ожидалось Absent");
        };
        let k = BlobKey {
            tenant: t,
            hash: h,
            generation,
        };
        store.put(&k, data.clone()).await.expect("ok");
        idx.commit_new(t, &h, generation, data.len() as u64, 2)
            .await
            .expect("ok");
        idx.release(t, &h, generation, 2).await.expect("ok");

        // Объект-«сирота»: загружен, но не учтён.
        let orphan = key(9, Gen::INITIAL);
        store.put(&orphan, data.clone()).await.expect("ok");

        clock.advance(W * 2);
        let older = clock.now() - W;
        for key in idx.gc_candidates(older, 100).await.expect("ok") {
            if idx.mark_deleting(&key, older).await.expect("ok") {
                store.delete(&key).await.expect("ok");
                assert!(idx.finalize_delete(&key).await.expect("ok"));
            }
        }
        let mut orphans = Vec::new();
        let entries: Vec<_> = store.list("", older).try_collect().await.expect("ok");
        for e in entries {
            if let Some(key) = e.key()
                && !idx.exists(&key).await.expect("ok")
            {
                orphans.push(key);
            }
        }
        assert_eq!(orphans, [orphan]);
        assert!(!store.contains(&k));
    }
}
