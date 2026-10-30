//! BLOB-08: гонка GC × дедупликация × поиск «сирот» (SRS 3.4).
//!
//! Настоящие [`BlobWriter`], [`GcWorker`] и [`OrphanSweeper`] работают задачами tokio поверх
//! testkit; каждое их обращение к учёту и хранилищу проходит через «шлюз» своей задачи.
//! proptest выбирает, какой задаче сделать следующий шаг, — так перебираются чередования шагов
//! протокола (acquire / head / put / commit_new, gc_candidates / mark_deleting / delete /
//! finalize_delete, list / exists / delete), снятия ссылок и хода часов.
//!
//! Инварианты: объект с живой ссылкой читается после каждого шага; после завершения задач и
//! прохода поиска «сирот» в хранилище нет объектов без записи учёта; после снятия всех ссылок и GC
//! хранилище пусто.
//!
//! Модель времени — допущение протокола: загрузка длится меньше периода ожидания «сирот».
//! Писатель, не завершивший запись за `MAX_UPLOAD`, обрывается (как упавший узел).
//!
//! PostgreSQL-вариант (учёт `tp-meta` при `TEST_DATABASE_URL`) не подключён: крейт полосы blob
//! не зависит от `tp-meta`, а время учёта в PostgreSQL — часы СУБД, не управляемые тестом.

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use proptest::prelude::*;
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tp_blob::{BlobWriter, GcConfig, GcWorker, Now, OrphanConfig, OrphanSweeper, WriterConfig};
use tp_contracts::blob::{Acquire, BlobIndex, BlobStore, ByteStream, Commit, ObjectStream};
use tp_contracts::{BlobHash, BlobKey, Gen, Result, TenantId};
use tp_testkit::blob::{BlobState, MemBlobIndex, MemBlobStore};
use tp_testkit::{Clock, ManualClock};

const T: TenantId = TenantId::DEFAULT;
const W_SECS: u64 = 20;
const GRACE_SECS: u64 = 60;
const MAX_UPLOAD_SECS: u64 = 30;
const BODIES: [&[u8]; 3] = [b"alpha", b"beta", b"gamma"];

// ---------- Шлюз ----------

/// Точка чередования: задача сообщает о приходе и ждёт разрешения на одно обращение.
#[derive(Clone, Default)]
struct Gate(Arc<GateInner>);

struct GateInner {
    arrived: Semaphore,
    go: Semaphore,
}

impl Default for GateInner {
    fn default() -> Self {
        Self {
            arrived: Semaphore::new(0),
            go: Semaphore::new(0),
        }
    }
}

impl Gate {
    async fn pass(&self) {
        self.0.arrived.add_permits(1);
        if let Ok(p) = self.0.go.acquire().await {
            p.forget();
        }
    }
}

struct GatedIndex {
    inner: Arc<dyn BlobIndex>,
    gate: Gate,
}

#[async_trait]
impl BlobIndex for GatedIndex {
    async fn acquire(&self, t: TenantId, h: &BlobHash, refs: u32) -> Result<Acquire> {
        self.gate.pass().await;
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
        self.gate.pass().await;
        self.inner.commit_new(t, h, generation, size, refs).await
    }
    async fn release(&self, t: TenantId, h: &BlobHash, g: Gen, refs: u32) -> Result<()> {
        self.gate.pass().await;
        self.inner.release(t, h, g, refs).await
    }
    async fn gc_candidates(&self, o: OffsetDateTime, l: usize) -> Result<Vec<BlobKey>> {
        self.gate.pass().await;
        self.inner.gc_candidates(o, l).await
    }
    async fn mark_deleting(&self, k: &BlobKey, o: OffsetDateTime) -> Result<bool> {
        self.gate.pass().await;
        self.inner.mark_deleting(k, o).await
    }
    async fn finalize_delete(&self, k: &BlobKey) -> Result<bool> {
        self.gate.pass().await;
        self.inner.finalize_delete(k).await
    }
    async fn exists(&self, k: &BlobKey) -> Result<bool> {
        self.gate.pass().await;
        self.inner.exists(k).await
    }
}

struct GatedStore {
    inner: Arc<dyn BlobStore>,
    gate: Gate,
}

#[async_trait]
impl BlobStore for GatedStore {
    async fn put(&self, key: &BlobKey, data: Bytes) -> Result<()> {
        self.gate.pass().await;
        self.inner.put(key, data).await
    }
    async fn put_stream(&self, key: &BlobKey, data: ByteStream, hint: Option<u64>) -> Result<()> {
        self.gate.pass().await;
        self.inner.put_stream(key, data, hint).await
    }
    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream> {
        self.gate.pass().await;
        self.inner.get(key, range).await
    }
    async fn head(&self, key: &BlobKey) -> Result<Option<u64>> {
        self.gate.pass().await;
        self.inner.head(key).await
    }
    async fn delete(&self, key: &BlobKey) -> Result<()> {
        self.gate.pass().await;
        self.inner.delete(key).await
    }
    fn list(&self, prefix: &str, older_than: OffsetDateTime) -> ObjectStream {
        // Снимок листинга — в момент разрешённого шага, а не вызова.
        let gate = self.gate.clone();
        let inner = Arc::clone(&self.inner);
        let prefix = prefix.to_owned();
        stream::once(async move {
            gate.pass().await;
            inner.list(&prefix, older_than)
        })
        .flatten()
        .boxed()
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

// ---------- Задачи ----------

/// Итог задачи: ключ записи с числом ссылок или ничего (GC, «сироты», неудачная запись).
type Outcome = Option<(BlobKey, u32)>;

struct Actor {
    gate: Gate,
    task: JoinHandle<Outcome>,
    /// Задача стоит у шлюза и ждёт разрешения.
    parked: bool,
    started: OffsetDateTime,
    writer: bool,
}

enum Step {
    Parked,
    Done(Outcome),
}

impl Actor {
    /// Дожидается, пока задача встанет у шлюза или завершится.
    async fn settle(&mut self) -> Step {
        if self.parked {
            return Step::Parked;
        }
        tokio::select! {
            biased;
            p = self.gate.0.arrived.acquire() => {
                if let Ok(p) = p {
                    p.forget();
                }
                self.parked = true;
                Step::Parked
            }
            r = &mut self.task => Step::Done(r.ok().flatten()),
        }
    }

    /// Разрешает одно обращение и дожидается следующей остановки.
    async fn step(&mut self) -> Step {
        if let Step::Done(r) = self.settle().await {
            return Step::Done(r);
        }
        self.parked = false;
        self.gate.0.go.add_permits(1);
        self.settle().await
    }
}

struct World {
    clock: ManualClock,
    index: MemBlobIndex,
    store: MemBlobStore,
    actors: Vec<Actor>,
    gc: Option<Actor>,
    sweeper: Option<Actor>,
    /// Живые ссылки завершённых записей.
    held: Vec<(BlobKey, u32)>,
}

impl World {
    fn new() -> Self {
        let clock = ManualClock::new(OffsetDateTime::UNIX_EPOCH + time::Duration::days(1000));
        Self {
            index: MemBlobIndex::new(clock.shared()),
            store: MemBlobStore::new(clock.shared()),
            clock,
            actors: Vec::new(),
            gc: None,
            sweeper: None,
            held: Vec::new(),
        }
    }

    fn now(&self) -> Now {
        let c = self.clock.clone();
        Arc::new(move || c.now())
    }

    fn gated(&self) -> (Gate, Arc<dyn BlobIndex>, Arc<dyn BlobStore>) {
        let gate = Gate::default();
        let index = Arc::new(GatedIndex {
            inner: Arc::new(self.index.clone()),
            gate: gate.clone(),
        });
        let store = Arc::new(GatedStore {
            inner: Arc::new(self.store.clone()),
            gate: gate.clone(),
        });
        (gate, index, store)
    }

    fn actor(&self, gate: Gate, task: JoinHandle<Outcome>, writer: bool) -> Actor {
        Actor {
            gate,
            task,
            parked: false,
            started: self.clock.now(),
            writer,
        }
    }

    fn spawn_writer(&mut self, body: usize, refs: u32) {
        let (gate, index, store) = self.gated();
        let writer = BlobWriter::new(index, store, WriterConfig::default());
        let data = Bytes::from_static(BODIES[body % BODIES.len()]);
        let task =
            tokio::spawn(async move { writer.write(T, data, refs).await.ok().map(|k| (k, refs)) });
        let actor = self.actor(gate, task, true);
        self.actors.push(actor);
    }

    fn spawn_gc(&self) -> Actor {
        let (gate, index, store) = self.gated();
        let gc = GcWorker::new(
            index,
            store,
            GcConfig {
                window_secs: W_SECS,
                batch: 2,
                max_deletes_per_sec: 0,
                ..GcConfig::default()
            },
            self.now(),
        );
        let task = tokio::spawn(async move {
            gc.run_once().await.ok();
            None
        });
        self.actor(gate, task, false)
    }

    fn spawn_sweeper(&self) -> Actor {
        let (gate, index, store) = self.gated();
        let sweeper = OrphanSweeper::new(
            index,
            store,
            OrphanConfig {
                grace_secs: GRACE_SECS,
                max_checks_per_sec: 0,
                max_deletes_per_sec: 0,
                ..OrphanConfig::default()
            },
            self.now(),
        );
        let task = tokio::spawn(async move {
            sweeper.run_once().await.ok();
            None
        });
        self.actor(gate, task, false)
    }

    fn finish(&mut self, outcome: Outcome) {
        if let Some(held) = outcome {
            self.held.push(held);
        }
    }

    async fn step_writer(&mut self, i: usize) {
        if self.actors.is_empty() {
            return;
        }
        let i = i % self.actors.len();
        if let Step::Done(r) = self.actors[i].step().await {
            self.actors.remove(i);
            self.finish(r);
        }
    }

    async fn step_gc(&mut self) {
        let mut gc = match self.gc.take() {
            Some(gc) => gc,
            None => self.spawn_gc(),
        };
        if let Step::Parked = gc.step().await {
            self.gc = Some(gc);
        }
    }

    async fn step_sweeper(&mut self) {
        let mut sweeper = match self.sweeper.take() {
            Some(s) => s,
            None => self.spawn_sweeper(),
        };
        if let Step::Parked = sweeper.step().await {
            self.sweeper = Some(sweeper);
        }
    }

    /// След оборванной записи: объект по ключу, который `acquire` выдал бы сейчас, без `commit_new`.
    async fn leftover(&mut self, body: usize) {
        let data = Bytes::from_static(BODIES[body % BODIES.len()]);
        let hash = tp_blob::BlobHasher::digest(&data);
        let generation = match self.index.row(T, &hash) {
            None => Gen::INITIAL,
            Some(r) if r.state != BlobState::Live => r.generation.next(),
            Some(_) => return,
        };
        let key = BlobKey {
            tenant: T,
            hash,
            generation,
        };
        self.store.put(&key, data).await.expect("put");
    }

    async fn release(&mut self, i: usize) {
        if self.held.is_empty() {
            return;
        }
        let i = i % self.held.len();
        let (key, refs) = &mut self.held[i];
        self.index
            .release(key.tenant, &key.hash, key.generation, 1)
            .await
            .expect("снятие живой ссылки");
        *refs -= 1;
        if *refs == 0 {
            self.held.remove(i);
        }
    }

    /// Ход часов; писатели дольше `MAX_UPLOAD` обрываются (до `commit_new` ссылок у них нет).
    async fn advance(&mut self, secs: u64) {
        // Задачи, ещё не дошедшие до шлюза, сначала останавливаются: обрыв — только у шлюза.
        let mut parked = Vec::new();
        for mut a in std::mem::take(&mut self.actors) {
            match a.settle().await {
                Step::Done(r) => self.finish(r),
                Step::Parked => parked.push(a),
            }
        }
        self.clock.advance(Duration::from_secs(secs));
        let deadline = self.clock.now() - time::Duration::seconds(MAX_UPLOAD_SECS as i64);
        for a in parked {
            if a.writer && a.started <= deadline {
                a.task.abort();
            } else {
                self.actors.push(a);
            }
        }
    }

    /// Доводит все задачи до конца.
    async fn drain(&mut self) {
        while !self.actors.is_empty() {
            self.step_writer(0).await;
        }
        while self.gc.is_some() {
            self.step_gc().await;
        }
        while self.sweeper.is_some() {
            self.step_sweeper().await;
        }
    }

    /// Объект с живой ссылкой читается.
    fn check_held(&self) {
        for (key, _) in &self.held {
            assert!(
                self.store.contains(key),
                "объект с живой ссылкой пропал: {key}"
            );
            let row = self.index.row(key.tenant, &key.hash).expect("запись учёта");
            assert_eq!(row.generation, key.generation, "{key}");
            assert_eq!(row.state, BlobState::Live, "{key}");
        }
    }

    /// Ключи BLOB в хранилище.
    fn objects(&self) -> Vec<BlobKey> {
        self.store
            .paths()
            .iter()
            .filter_map(|p| BlobKey::parse_path(p).ok())
            .collect()
    }
}

#[derive(Debug, Clone)]
enum Op {
    Write { body: usize, refs: u32 },
    Leftover(usize),
    Step(usize),
    Release(usize),
    Gc,
    Sweep,
    Advance(u64),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..BODIES.len(), 1u32..3).prop_map(|(body, refs)| Op::Write { body, refs }),
        1 => (0..BODIES.len()).prop_map(Op::Leftover),
        8 => (0usize..8).prop_map(Op::Step),
        3 => (0usize..8).prop_map(Op::Release),
        3 => Just(Op::Gc),
        3 => Just(Op::Sweep),
        2 => (1u64..25).prop_map(Op::Advance),
    ]
}

async fn scenario(ops: Vec<Op>) {
    let mut w = World::new();
    for op in ops {
        match op {
            Op::Write { body, refs } => w.spawn_writer(body, refs),
            Op::Leftover(body) => w.leftover(body).await,
            Op::Step(i) => w.step_writer(i).await,
            Op::Release(i) => w.release(i).await,
            Op::Gc => w.step_gc().await,
            Op::Sweep => w.step_sweeper().await,
            Op::Advance(s) => w.advance(s).await,
        }
        w.check_held();
    }

    // Все задачи завершены; поиск «сирот» после периода ожидания оставляет только объекты с записью.
    w.drain().await;
    w.check_held();
    w.clock.advance(Duration::from_secs(GRACE_SECS + 1));
    w.step_sweeper().await;
    w.drain().await;
    w.check_held();
    for key in w.objects() {
        assert!(
            w.index.exists(&key).await.expect("вызов"),
            "«сирота» после цикла: {key}"
        );
    }

    // Все ссылки сняты, окно W истекло — GC удаляет всё.
    while !w.held.is_empty() {
        w.release(0).await;
    }
    w.clock.advance(Duration::from_secs(W_SECS + 1));
    w.step_gc().await;
    w.drain().await;
    assert_eq!(w.objects(), Vec::<BlobKey>::new(), "объекты после GC");
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn gc_dedup_orphans_race(ops in prop::collection::vec(op(), 1..80)) {
        runtime().block_on(scenario(ops));
    }
}

/// Гонка из SRS 3.4: запись в `deleting` во время загрузки того же содержимого — `Bumped`,
/// а `finalize_delete` GC не заменяет запись нового поколения.
#[test]
fn bumped_while_gc_deletes() {
    runtime().block_on(scenario(vec![
        // W1: acquire → Absent(0), head, put(0) — и ждёт перед commit_new.
        Op::Write { body: 0, refs: 1 },
        Op::Step(0),
        Op::Step(0),
        Op::Step(0),
        // W2: ключ 0 занят объектом W1 — поколение 1; запись завершена.
        Op::Write { body: 0, refs: 1 },
        Op::Step(1), // acquire → Absent(0)
        Op::Step(1), // head(0): занят
        Op::Step(1), // head(1)
        Op::Step(1), // put(1)
        Op::Step(1), // commit_new(1) → Done(1)
        // Ссылка W2 снята, окно W истекло; GC переводит запись поколения 1 в `deleting`.
        Op::Release(0),
        Op::Advance(W_SECS + 1),
        Op::Gc, // gc_candidates
        Op::Gc, // mark_deleting(1)
        // W1: commit_new(0) → Bumped(2), загрузка поколения 2.
        Op::Step(0),
        Op::Step(0),
        Op::Step(0),
        Op::Step(0),
        // GC удаляет объект поколения 1; finalize_delete не трогает живую запись поколения 2.
        Op::Gc,
        Op::Gc,
    ]));
}

/// Повторное использование ключа оборванной записи при устаревшем листинге «сирот»:
/// без проверки занятого ключа в писателе поиск «сирот» удалил бы объект живой записи.
#[test]
fn reused_key_vs_stale_listing() {
    runtime().block_on(scenario(vec![
        // Объект оборванной записи поколения 0, старше периода ожидания.
        Op::Leftover(1),
        Op::Advance(GRACE_SECS + 1),
        Op::Sweep, // list: объект в листинге
        // Новая запись того же содержимого: acquire снова выдаёт поколение 0.
        Op::Write { body: 1, refs: 1 },
        Op::Step(0), // acquire → Absent(0)
        Op::Step(0), // head(0): занят — поколение 1 (без проверки — put(0), объект не обновлён)
        Op::Sweep,   // exists(0) → нет
        Op::Sweep,   // delete(0)
        Op::Step(0), // head(1) (без проверки — commit_new(0) → ссылка на удалённый объект)
        Op::Step(0), // put(1)
        Op::Step(0), // commit_new(1)
    ]));
}
