//! GC объектов BLOB (SRS 3.4, 3.5, 3.6): записи без ссылок старше окна W удаляются из хранилища.
//!
//! Проход: [`BlobIndex::prune_uploads`] (истёкшие отметки загрузок) → [`BlobIndex::gc_candidates`] пакетами →
//! [`BlobIndex::mark_deleting`] (условие `refcnt = 0` и окно W проверяются одной транзакцией) →
//! [`BlobStore::delete`] → [`BlobIndex::finalize_delete`].
//! Окно W не меньше глубины самой старой точки восстановления (инвариант W). Работает один узел —
//! держатель лизы лидера; лиза продлевается между пакетами, потеря лизы прекращает проход.
//! Сбой удаления оставляет запись `deleting` — следующий проход повторит её.
//!
//! Шарды (SRS 3.6, решение 2026-11-01-scale-shards-geo-admin): учёт ссылок ведётся на каждом шарде, объект
//! удаляется, только когда на него не ссылается ни один шард справочника. Проверку выполняет учёт: операции GC
//! контракта [`BlobIndex`] работают по всем шардам справочника (`tp-meta`: пометка `deleting` под блокировкой
//! строки каждого шарда, надгробие шарду без строки). Воркеру передаётся учёт всех шардов, а не одного; отказ
//! `mark_deleting` — запрет удаления (ссылки на этом или другом шарде, окно W шарда не истекло). Проход
//! не полагается на свойства учёта одного шарда: пакет короче предела не означает конец кандидатов (учёт
//! отбрасывает занятые другим шардом), а кандидат с запретом может вернуться в следующем пакете — он
//! запоминается на проход и повторно не проверяется.
//!
//! Воркер ([`GcWorker::run`]) не делает периодических проходов (SRS 2.8, таймер 4): один таймер на ближайший
//! срок окончания отсрочки — [`BlobIndex::next_gc_due`] (`zero_since + W` самой ранней записи без ссылок).
//! Ожидание не дольше `min(max_wait, window)`: по его истечении срок сверяется заново (без прохода).
//! Ссылки, обнулённые после взвода, дают срок не раньше `now + W`, то есть не раньше взведённого таймера, —
//! поэтому обнуление на любом узле без сигнала не опаздывает; сигнал [`GcWorker::wake`] после фиксации
//! обнуления в этом процессе перевзводит таймер сразу. Просроченное после прохода или ошибки — повтор
//! не раньше [`GcConfig::retry`] (таймер 2), лиза у другого узла — не раньше [`GcConfig::lease_ttl`] (таймер 1).

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::Notify;
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::config::duration;
use tp_contracts::lease::{Lease, LeaseStore};
use tp_contracts::{BlobKey, Result};

use crate::worker::{
    Leader, LeasedPass, Now, Pacer, Schedule, Sleep, Tick, after, before, keep, release,
    system_sleep, tick,
};

/// Конфигурация GC (секция config.yaml; поля необязательны, неизвестное — ошибка).
///
/// ```yaml
/// window: 7d
/// batch: 1000
/// max_deletes_per_sec: 200
/// max_wait: 1h
/// retry: 5m
/// lease_name: blob-gc
/// lease_ttl: 2m
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GcConfig {
    /// Окно W: объект удаляется не раньше, чем через это время после обнуления ссылок.
    /// Не меньше глубины самой старой точки восстановления (PITR, DR-реплика, снапшоты).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub window: Duration,
    /// Размер пакета кандидатов и истёкших отметок загрузок.
    pub batch: usize,
    /// Предел удалений в секунду; 0 — без ограничения.
    pub max_deletes_per_sec: u32,
    /// Наибольшее ожидание таймера (не больше `window`): по истечении ближайший срок сверяется заново.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub max_wait: Duration,
    /// Пауза перед повтором после ошибки или если кандидаты остались после прохода.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry: Duration,
    /// Имя лизы лидера GC.
    pub lease_name: String,
    /// Срок лизы; продлевается между пакетами.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub lease_ttl: Duration,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(7 * 24 * 3600),
            batch: 1000,
            max_deletes_per_sec: 200,
            max_wait: Duration::from_secs(3600),
            retry: Duration::from_secs(300),
            lease_name: "blob-gc".to_owned(),
            lease_ttl: Duration::from_secs(120),
        }
    }
}

/// Итог одного прохода.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcPass {
    /// Удалено истёкших отметок загрузок.
    pub uploads_pruned: u64,
    /// Получено кандидатов.
    pub candidates: u64,
    /// Удалено объектов (запись заменена надгробием или уже другого поколения).
    pub deleted: u64,
    /// Кандидатов, которых удалять нельзя: ссылки вернулись (на любом шарде) или поколение сменилось.
    pub skipped: u64,
    /// Удалённых объектов, чью запись `finalize_delete` не заменил: запись уже другого поколения.
    pub conflicts: u64,
    /// Проход прекращён: лиза потеряна.
    pub lease_lost: bool,
}

/// Снимок метрик GC (накопительные счётчики).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcMetrics {
    /// Завершённых проходов.
    pub passes: u64,
    /// Удалённых истёкших отметок загрузок.
    pub uploads_pruned: u64,
    /// Кандидатов.
    pub candidates: u64,
    /// Удалённых объектов.
    pub deleted: u64,
    /// Пропущенных кандидатов.
    pub skipped: u64,
    /// Конфликтов `finalize_delete`.
    pub conflicts: u64,
    /// Ошибок (учёт или хранилище недоступны).
    pub errors: u64,
    /// Потерь лизы во время прохода.
    pub lease_lost: u64,
}

#[derive(Debug, Default)]
struct Counters {
    passes: AtomicU64,
    uploads_pruned: AtomicU64,
    candidates: AtomicU64,
    deleted: AtomicU64,
    skipped: AtomicU64,
    conflicts: AtomicU64,
    errors: AtomicU64,
    lease_lost: AtomicU64,
}

fn inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Воркер GC.
pub struct GcWorker {
    index: Arc<dyn BlobIndex>,
    store: Arc<dyn BlobStore>,
    cfg: GcConfig,
    now: Now,
    sleep: Sleep,
    wake: Notify,
    counters: Counters,
}

impl std::fmt::Debug for GcWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcWorker")
            .field("cfg", &self.cfg)
            .field("metrics", &self.metrics())
            .finish_non_exhaustive()
    }
}

impl GcWorker {
    /// Воркер поверх учёта `index` и хранилища `store`; `now` — часы UTC ([`crate::system_now`]).
    /// `index` — учёт всех шардов справочника (SRS 3.6): учёт одного шарда удалил бы объект,
    /// на который ссылается другой.
    pub fn new(
        index: Arc<dyn BlobIndex>,
        store: Arc<dyn BlobStore>,
        cfg: GcConfig,
        now: Now,
    ) -> Self {
        let sleep = system_sleep(now.clone());
        Self {
            index,
            store,
            cfg,
            now,
            sleep,
            wake: Notify::new(),
            counters: Counters::default(),
        }
    }

    /// Таймер воркера (тесты: по часам testkit).
    #[cfg(test)]
    fn with_sleep(mut self, sleep: Sleep) -> Self {
        self.sleep = sleep;
        self
    }

    /// Сигнал после фиксации обнуления ссылок (или любого изменения учёта): воркер сверит ближайший
    /// срок и перевзведёт таймер. Не обязателен для корректности — без сигнала срок сверяется
    /// не реже `min(max_wait, window)`.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Снимок метрик.
    pub fn metrics(&self) -> GcMetrics {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        GcMetrics {
            passes: get(&c.passes),
            uploads_pruned: get(&c.uploads_pruned),
            candidates: get(&c.candidates),
            deleted: get(&c.deleted),
            skipped: get(&c.skipped),
            conflicts: get(&c.conflicts),
            errors: get(&c.errors),
            lease_lost: get(&c.lease_lost),
        }
    }

    /// Разовый проход без лизы (команда обслуживания, тесты). Вызывающий отвечает за то,
    /// что GC не идёт параллельно на другом узле.
    pub async fn run_once(&self) -> Result<GcPass> {
        self.pass(None).await
    }

    /// Проходы под лизой лидера по ближайшему сроку до `shutdown`; `holder` — идентификатор узла.
    /// Футура — `Send` (при `Send`-футуре `shutdown`): запускается через `tokio::spawn`.
    pub async fn run(
        &self,
        leases: &dyn LeaseStore,
        holder: &str,
        shutdown: impl Future<Output = ()>,
    ) {
        let s = Schedule {
            leases,
            name: &self.cfg.lease_name,
            holder,
            ttl: self.cfg.lease_ttl,
            what: "blob-gc",
        };
        tokio::pin!(shutdown);
        let mut held: Option<Lease> = None;
        // Просроченное — не раньше этого момента (после прохода, ошибки или без лизы): иначе
        // необработанный кандидат взводил бы таймер в прошлое.
        let mut floor: Option<OffsetDateTime> = None;
        loop {
            let now = (self.now)();
            let wait = self.cfg.max_wait.min(self.cfg.window);
            let cap = after(now, wait.max(Duration::from_millis(1)));
            let (at, pass) = match self.next_due(now, cap).await {
                Ok(Some(due)) if due <= now => (floor.map_or(now, |f| f.max(now)), true),
                Ok(Some(due)) => (due, true),
                Ok(None) => (cap, false),
                Err(e) => {
                    inc(&self.counters.errors);
                    tracing::warn!(error = %e, "ближайший срок GC BLOB не получен");
                    (after(now, self.cfg.retry), false)
                }
            };
            tokio::select! {
                biased;
                () = &mut shutdown => break,
                // Изменение учёта в этом процессе: сверить срок и перевзвести таймер.
                () = self.wake.notified() => {}
                () = (self.sleep)(at), if !pass => {}
                () = (self.sleep)(at), if pass => {
                    let outcome = tick(s, &mut held, self).await;
                    let now = (self.now)();
                    floor = Some(match outcome {
                        Tick::NotLeader => after(now, self.cfg.lease_ttl),
                        Tick::Done | Tick::Failed => after(now, self.cfg.retry),
                    });
                }
            }
        }
        release(s, held).await;
    }

    /// Ближайший срок удаления, если он не позже `cap`; просроченные кандидаты — срок `now`.
    /// Срок записи по учёту — `zero_since + W`, но условие удаления строгое (`zero_since < now − W`):
    /// проход должен быть позже срока, поэтому таймер — на микросекунду (разрешение времени учёта) после него.
    async fn next_due(
        &self,
        now: OffsetDateTime,
        cap: OffsetDateTime,
    ) -> Result<Option<OffsetDateTime>> {
        let due = self.index.next_gc_due(self.cfg.window).await?;
        Ok(due
            .map(|d| after(d, Duration::from_micros(1)))
            .filter(|d| *d <= cap)
            .map(|d| d.max(now)))
    }

    async fn pass(&self, mut leader: Option<&mut Leader<'_>>) -> Result<GcPass> {
        let res = self.pass_inner(&mut leader).await;
        match &res {
            Ok(p) => {
                inc(&self.counters.passes);
                if p.lease_lost {
                    inc(&self.counters.lease_lost);
                }
                tracing::info!(
                    uploads_pruned = p.uploads_pruned,
                    candidates = p.candidates,
                    deleted = p.deleted,
                    skipped = p.skipped,
                    conflicts = p.conflicts,
                    "проход GC BLOB"
                );
            }
            Err(e) => {
                inc(&self.counters.errors);
                tracing::warn!(error = %e, "проход GC BLOB прерван");
            }
        }
        res
    }

    async fn pass_inner(&self, leader: &mut Option<&mut Leader<'_>>) -> Result<GcPass> {
        let now = (self.now)();
        // Граница окна фиксируется на проход: кандидаты и `mark_deleting` проверяют одно условие.
        let older_than = before(now, self.cfg.window);
        let batch = self.cfg.batch.max(1);
        let mut pacer = Pacer::new(self.cfg.max_deletes_per_sec);
        let mut pass = GcPass::default();
        // Отметки загрузок, срок которых истёк (контракт: очищает GC в каждом проходе).
        loop {
            let pruned = self.index.prune_uploads(now, batch).await?;
            pass.uploads_pruned += pruned as u64;
            self.counters
                .uploads_pruned
                .fetch_add(pruned as u64, Ordering::Relaxed);
            if pruned < batch {
                break;
            }
            if !keep(leader).await? {
                pass.lease_lost = true;
                return Ok(pass);
            }
        }
        // Кандидаты, удаление которых учёт запретил в этом проходе (ссылки на любом шарде).
        let mut vetoed: HashSet<BlobKey> = HashSet::new();
        loop {
            let keys = self.index.gc_candidates(older_than, batch).await?;
            let mut progress = false;
            for key in &keys {
                if vetoed.contains(key) {
                    continue;
                }
                pass.candidates += 1;
                inc(&self.counters.candidates);
                if !self.index.mark_deleting(key, older_than).await? {
                    vetoed.insert(*key);
                    pass.skipped += 1;
                    inc(&self.counters.skipped);
                    progress = true;
                    continue;
                }
                pacer.wait().await;
                self.store.delete(key).await?;
                if self.index.finalize_delete(key).await? {
                    tracing::debug!(%key, "объект BLOB удалён GC");
                    progress = true;
                } else {
                    pass.conflicts += 1;
                    inc(&self.counters.conflicts);
                }
                pass.deleted += 1;
                inc(&self.counters.deleted);
            }
            // Конец — пустой пакет: неполный пакет концом не считается (учёт всех шардов отбрасывает кандидатов,
            // занятых другим шардом, уже после выборки). Пакет без продвижения (только уже запрещённые
            // кандидаты или незаменённые записи) — защита от зацикливания на одних и тех же кандидатах.
            if !progress {
                break;
            }
            if !keep(leader).await? {
                pass.lease_lost = true;
                break;
            }
        }
        Ok(pass)
    }
}

impl LeasedPass for GcWorker {
    async fn leased_pass(&self, leader: &mut Leader<'_>) -> Result<()> {
        self.pass(Some(leader)).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use time::OffsetDateTime;
    use tp_contracts::blob::{Acquire, Commit};
    use tp_contracts::meta::AccountScope;
    use tp_contracts::{AccountId, BlobHash, Error, Gen, TenantId};
    use tp_testkit::blob::{BlobOp, BlobRow, BlobState, MemBlobIndex, MemBlobStore};
    use tp_testkit::{Clock, ManualClock, MemLease};

    use crate::writer::{BlobWriter, WriterConfig};

    const T: TenantId = TenantId::DEFAULT;
    const W: u64 = 3600;

    struct Env {
        clock: ManualClock,
        index: MemBlobIndex,
        store: MemBlobStore,
        writer: BlobWriter,
        gc: Arc<GcWorker>,
    }

    fn env(cfg: GcConfig) -> Env {
        let clock = ManualClock::new(OffsetDateTime::UNIX_EPOCH + time::Duration::days(1000));
        let index = MemBlobIndex::new(clock.shared());
        let store = MemBlobStore::new(clock.shared());
        let writer = BlobWriter::new(
            Arc::new(index.clone()),
            Arc::new(store.clone()),
            WriterConfig::default(),
        );
        let c = clock.clone();
        let gc = Arc::new(GcWorker::new(
            Arc::new(index.clone()),
            Arc::new(store.clone()),
            GcConfig {
                window: Duration::from_secs(W),
                max_deletes_per_sec: 0,
                ..cfg
            },
            Arc::new(move || c.now()),
        ));
        Env {
            clock,
            index,
            store,
            writer,
            gc,
        }
    }

    async fn put(e: &Env, body: &'static str, refs: u32) -> BlobKey {
        e.writer
            .write(T, Bytes::from_static(body.as_bytes()), refs)
            .await
            .expect("вызов")
    }

    #[tokio::test]
    async fn referenced_object_stays() {
        let e = env(GcConfig::default());
        let kept = put(&e, "kept", 2).await;
        e.index
            .release(T, &kept.hash, kept.generation, 1)
            .await
            .expect("вызов");
        e.clock.advance(Duration::from_secs(10 * W));
        assert_eq!(e.gc.run_once().await.expect("вызов"), GcPass::default());
        assert!(e.store.contains(&kept));
        assert_eq!(
            e.index.row(T, &kept.hash).expect("значение").state,
            BlobState::Live
        );
    }

    #[tokio::test]
    async fn deleted_after_window() {
        let e = env(GcConfig::default());
        let key = put(&e, "gone", 1).await;
        e.index
            .release(T, &key.hash, key.generation, 1)
            .await
            .expect("вызов");

        // До истечения W объект на месте.
        e.clock.advance(Duration::from_secs(W - 1));
        assert_eq!(e.gc.run_once().await.expect("вызов").deleted, 0);
        assert!(e.store.contains(&key));

        e.clock.advance(Duration::from_secs(2));
        let pass = e.gc.run_once().await.expect("вызов");
        assert_eq!((pass.candidates, pass.deleted), (1, 1));
        assert!(!e.store.contains(&key));
        assert_eq!(
            e.index.row(T, &key.hash).expect("значение").state,
            BlobState::Deleted
        );
        assert!(!e.index.exists(&key).await.expect("вызов"));

        // Повторная запись того же содержимого — следующее поколение.
        let again = put(&e, "gone", 1).await;
        assert_eq!(again.generation, Gen(1));
        assert!(e.store.contains(&again));

        let m = e.gc.metrics();
        assert_eq!((m.passes, m.candidates, m.deleted, m.errors), (2, 1, 1, 0));
    }

    #[tokio::test]
    async fn batches_and_metrics() {
        let e = env(GcConfig {
            batch: 2,
            ..GcConfig::default()
        });
        for body in ["a", "b", "c", "d", "e"] {
            let key = put(&e, body, 1).await;
            e.index
                .release(T, &key.hash, key.generation, 1)
                .await
                .expect("вызов");
        }
        e.clock.advance(Duration::from_secs(W + 1));
        let pass = e.gc.run_once().await.expect("вызов");
        assert_eq!((pass.candidates, pass.deleted), (5, 5));
        assert!(e.store.paths().is_empty());
        assert_eq!(e.gc.metrics().deleted, 5);
    }

    #[tokio::test]
    async fn reacquired_candidate_skipped() {
        let e = env(GcConfig::default());
        let key = put(&e, "back", 1).await;
        e.index
            .release(T, &key.hash, key.generation, 1)
            .await
            .expect("вызов");
        e.clock.advance(Duration::from_secs(W + 1));
        let older_than = e.clock.now();
        let candidates = e.index.gc_candidates(older_than, 10).await.expect("вызов");
        assert_eq!(candidates, vec![key]);
        // Между выборкой кандидатов и `mark_deleting` пришло письмо с тем же вложением.
        assert_eq!(
            e.index.acquire(T, None, &key.hash, 1).await.expect("вызов"),
            Acquire::Existing {
                generation: key.generation
            }
        );
        assert!(
            !e.index
                .mark_deleting(&key, older_than)
                .await
                .expect("вызов")
        );
        assert_eq!(e.gc.run_once().await.expect("вызов").deleted, 0);
        assert!(e.store.contains(&key));
    }

    #[tokio::test]
    async fn delete_error_retried_next_pass() {
        let e = env(GcConfig::default());
        let key = put(&e, "retry", 1).await;
        e.index
            .release(T, &key.hash, key.generation, 1)
            .await
            .expect("вызов");
        e.clock.advance(Duration::from_secs(W + 1));
        e.store.fail_next(BlobOp::Delete, Error::UNAVAILABLE);
        assert_eq!(e.gc.run_once().await, Err(Error::UNAVAILABLE));
        assert_eq!(
            e.index.row(T, &key.hash).expect("значение").state,
            BlobState::Deleting
        );
        assert_eq!(e.gc.metrics().errors, 1);

        assert_eq!(e.gc.run_once().await.expect("вызов").deleted, 1);
        assert!(!e.store.contains(&key));
        assert_eq!(
            e.index.row(T, &key.hash).expect("значение").state,
            BlobState::Deleted
        );
    }

    #[tokio::test]
    async fn run_under_lease() {
        let e = env(GcConfig::default());
        let key = put(&e, "leader", 1).await;
        e.index
            .release(T, &key.hash, key.generation, 1)
            .await
            .expect("вызов");
        e.clock.advance(Duration::from_secs(W + 1));

        let leases = MemLease::new(e.clock.shared());
        // Лизу держит другой узел — проход пропускается.
        let other = leases
            .try_acquire("blob-gc", "node-b", Duration::from_secs(600))
            .await
            .expect("вызов")
            .expect("значение");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let run = e.gc.run(&leases, "node-a", async {
            rx.await.ok();
        });
        let check = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(e.store.contains(&key));
            tx.send(()).ok();
        };
        tokio::join!(run, check);

        // Лиза свободна — проход идёт, по завершении лиза освобождена.
        leases.release(&other).await.expect("вызов");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let run = e.gc.run(&leases, "node-a", async {
            rx.await.ok();
        });
        let check = async {
            for _ in 0..100 {
                if !e.store.contains(&key) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(leases.holder("blob-gc").as_deref(), Some("node-a"));
            tx.send(()).ok();
        };
        tokio::join!(run, check);
        assert!(!e.store.contains(&key));
        assert_eq!(leases.holder("blob-gc"), None);
    }

    /// Фоновая работа: `run` — `Send`-футура, задача tokio выполняет проход под лизой и останавливается.
    #[tokio::test]
    async fn run_in_spawned_task() {
        let e = env(GcConfig::default());
        let key = put(&e, "spawned", 1).await;
        e.index
            .release(T, &key.hash, key.generation, 1)
            .await
            .expect("вызов");
        e.clock.advance(Duration::from_secs(W + 1));

        let leases = Arc::new(MemLease::new(e.clock.shared()));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let (gc, l) = (e.gc.clone(), leases.clone());
        let task = tokio::spawn(async move {
            gc.run(&*l, "node-a", async {
                rx.await.ok();
            })
            .await;
        });
        for _ in 0..100 {
            if !e.store.contains(&key) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tx.send(()).ok();
        task.await.expect("задача");
        assert!(!e.store.contains(&key));
        assert_eq!(leases.holder("blob-gc"), None);
    }

    #[tokio::test]
    async fn expired_upload_marks_pruned() {
        let e = env(GcConfig {
            batch: 2,
            ..GcConfig::default()
        });
        let scope = AccountScope {
            tenant: T,
            account: AccountId::new(),
        };
        let now = e.clock.now();
        let mut keys = Vec::new();
        for (body, ttl) in [("u1", 10), ("u2", 10), ("u3", 10), ("u4", 1000)] {
            let key = put(&e, body, 1).await;
            e.index
                .mark_upload(scope, &key, now + secs(ttl))
                .await
                .expect("вызов");
            keys.push(key);
        }
        e.clock.advance(Duration::from_secs(11));
        // Истёкших больше пакета — удаляются все; действующая отметка остаётся.
        let pass = e.gc.run_once().await.expect("вызов");
        assert_eq!((pass.uploads_pruned, pass.candidates), (3, 0));
        assert_eq!(e.gc.metrics().uploads_pruned, 3);
        let now = e.clock.now();
        assert!(
            e.index
                .upload_owned(scope, &keys[3], now)
                .await
                .expect("вызов")
        );
        assert_eq!(e.gc.run_once().await.expect("вызов").uploads_pruned, 0);
    }

    /// Учёт двух шардов в памяти по контракту [`BlobIndex`] для хранилища с шардами (как `tp-meta` со
    /// справочником): запись и снятие ссылок — на первом шарде, операции GC — по всем шардам.
    /// `filter` — `gc_candidates` отбрасывает кандидатов, занятых другим шардом (как `tp-meta`);
    /// без него запрет виден только в `mark_deleting`.
    #[derive(Debug)]
    struct Shards {
        shards: Vec<MemBlobIndex>,
        filter: bool,
    }

    impl Shards {
        /// Удалять нельзя: на каком-то шарде на объект есть ссылки или окно W ещё не истекло.
        fn busy(&self, key: &BlobKey, older_than: OffsetDateTime) -> bool {
            self.shards.iter().any(|s| {
                s.row(key.tenant, &key.hash).is_some_and(|r| {
                    r.generation == key.generation
                        && r.state == BlobState::Live
                        && (r.refcnt > 0 || r.zero_since.is_none_or(|z| z >= older_than))
                })
            })
        }
    }

    #[async_trait::async_trait]
    impl BlobIndex for Shards {
        async fn acquire(
            &self,
            tenant: TenantId,
            owner: Option<AccountId>,
            hash: &BlobHash,
            refs: u32,
        ) -> Result<Acquire> {
            self.shards[0].acquire(tenant, owner, hash, refs).await
        }

        async fn commit_new(
            &self,
            tenant: TenantId,
            hash: &BlobHash,
            generation: Gen,
            size: u64,
            refs: u32,
        ) -> Result<Commit> {
            self.shards[0]
                .commit_new(tenant, hash, generation, size, refs)
                .await
        }

        async fn release(
            &self,
            tenant: TenantId,
            hash: &BlobHash,
            generation: Gen,
            refs: u32,
        ) -> Result<()> {
            self.shards[0].release(tenant, hash, generation, refs).await
        }

        async fn gc_candidates(
            &self,
            older_than: OffsetDateTime,
            limit: usize,
        ) -> Result<Vec<BlobKey>> {
            let mut keys: Vec<BlobKey> = Vec::new();
            for s in &self.shards {
                for k in s.gc_candidates(older_than, limit).await? {
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
            }
            if self.filter {
                keys.retain(|k| !self.busy(k, older_than));
            }
            keys.truncate(limit);
            Ok(keys)
        }

        async fn mark_deleting(&self, key: &BlobKey, older_than: OffsetDateTime) -> Result<bool> {
            if self.busy(key, older_than) {
                return Ok(false);
            }
            let mut marked = false;
            for s in &self.shards {
                marked |= s.mark_deleting(key, older_than).await?;
            }
            Ok(marked)
        }

        async fn finalize_delete(&self, key: &BlobKey) -> Result<bool> {
            let mut done = false;
            for s in &self.shards {
                done |= s.finalize_delete(key).await?;
            }
            Ok(done)
        }

        async fn exists(&self, key: &BlobKey) -> Result<bool> {
            for s in &self.shards {
                if s.exists(key).await? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }

    /// Два шарда учёта над одним хранилищем: писатель — через первый шард, GC — по обоим.
    struct TwoShards {
        clock: ManualClock,
        store: MemBlobStore,
        index: [MemBlobIndex; 2],
        writer: BlobWriter,
        gc: GcWorker,
    }

    fn two_shards(filter: bool, batch: usize) -> TwoShards {
        let clock = ManualClock::new(OffsetDateTime::UNIX_EPOCH + time::Duration::days(1000));
        let store = MemBlobStore::new(clock.shared());
        let index = [
            MemBlobIndex::new(clock.shared()),
            MemBlobIndex::new(clock.shared()),
        ];
        let writer = BlobWriter::new(
            Arc::new(index[0].clone()),
            Arc::new(store.clone()),
            WriterConfig::default(),
        );
        let c = clock.clone();
        let gc = GcWorker::new(
            Arc::new(Shards {
                shards: index.to_vec(),
                filter,
            }),
            Arc::new(store.clone()),
            GcConfig {
                window: Duration::from_secs(W),
                max_deletes_per_sec: 0,
                batch,
                ..GcConfig::default()
            },
            Arc::new(move || c.now()),
        );
        TwoShards {
            clock,
            store,
            index,
            writer,
            gc,
        }
    }

    impl TwoShards {
        /// Объект с одной ссылкой на первом шарде.
        async fn put(&self, body: &'static str) -> BlobKey {
            self.writer
                .write(T, Bytes::from_static(body.as_bytes()), 1)
                .await
                .expect("вызов")
        }

        /// Ссылка второго шарда на тот же объект (письмо с ним в ящике этого шарда).
        async fn refer(&self, key: &BlobKey) {
            let size = self.row(0, key).size;
            assert_eq!(
                self.index[1]
                    .commit_new(T, &key.hash, key.generation, size, 1)
                    .await
                    .expect("вызов"),
                Commit::Done {
                    generation: key.generation
                }
            );
        }

        async fn release(&self, shard: usize, key: &BlobKey) {
            self.index[shard]
                .release(T, &key.hash, key.generation, 1)
                .await
                .expect("вызов");
        }

        fn row(&self, shard: usize, key: &BlobKey) -> BlobRow {
            self.index[shard].row(T, &key.hash).expect("значение")
        }

        /// Объект, на который ссылались оба шарда; ссылка первого снята, окно W истекло.
        async fn shared_released_on_first(&self) -> BlobKey {
            let key = self.put("shared").await;
            self.refer(&key).await;
            self.release(0, &key).await;
            self.clock.advance(Duration::from_secs(W + 1));
            key
        }
    }

    /// SRS 3.6: объект со ссылкой на одном шарде не удаляется, пока ссылку не снимут все шарды.
    #[tokio::test]
    async fn object_referenced_on_other_shard_stays() {
        let e = two_shards(true, 1000);
        let key = e.shared_released_on_first().await;

        let pass = e.gc.run_once().await.expect("вызов");
        assert_eq!(pass.deleted, 0);
        assert!(e.store.contains(&key));
        assert_eq!(e.row(0, &key).state, BlobState::Live);
        assert_eq!(
            (e.row(1, &key).state, e.row(1, &key).refcnt),
            (BlobState::Live, 1)
        );

        // Ссылку снял и второй шард: удаление — после окна W от этого момента.
        e.release(1, &key).await;
        e.clock.advance(Duration::from_secs(W - 1));
        assert_eq!(e.gc.run_once().await.expect("вызов").deleted, 0);
        assert!(e.store.contains(&key));
        e.clock.advance(Duration::from_secs(2));
        assert_eq!(e.gc.run_once().await.expect("вызов").deleted, 1);
        assert!(!e.store.contains(&key));
        assert_eq!(e.row(0, &key).state, BlobState::Deleted);
        assert_eq!(e.row(1, &key).state, BlobState::Deleted);
    }

    /// Учёт выдаёт кандидата, занятого другим шардом, в каждом пакете: запрет `mark_deleting` запоминается
    /// на проход — объект цел, проход завершается.
    #[tokio::test]
    async fn vetoed_candidate_checked_once_per_pass() {
        let e = two_shards(false, 1);
        let key = e.shared_released_on_first().await;

        let pass = e.gc.run_once().await.expect("вызов");
        assert_eq!((pass.candidates, pass.skipped, pass.deleted), (1, 1, 0));
        assert!(e.store.contains(&key));
        assert_eq!(e.row(0, &key).state, BlobState::Live);
        assert_eq!(e.row(1, &key).refcnt, 1);
    }

    /// Учёт отбрасывает занятого другим шардом кандидата уже после выборки — пакет короче предела;
    /// остальные кандидаты удаляются в этом же проходе.
    #[tokio::test]
    async fn short_batch_is_not_the_end() {
        let e = two_shards(true, 2);
        let shared = e.put("shared").await;
        e.refer(&shared).await;
        e.release(0, &shared).await;
        let mut free = Vec::new();
        for body in ["x", "y", "z"] {
            e.clock.advance(Duration::from_secs(1));
            let key = e.put(body).await;
            e.release(0, &key).await;
            free.push(key);
        }
        e.clock.advance(Duration::from_secs(W + 1));

        let pass = e.gc.run_once().await.expect("вызов");
        assert_eq!((pass.candidates, pass.deleted, pass.skipped), (3, 3, 0));
        assert!(free.iter().all(|k| !e.store.contains(k)));
        assert!(e.store.contains(&shared));
    }

    /// Таймер на часах testkit: запоминает взведённые сроки.
    fn test_sleep(
        clock: &ManualClock,
        armed: &Arc<std::sync::Mutex<Vec<OffsetDateTime>>>,
    ) -> Sleep {
        let (clock, armed) = (clock.clone(), armed.clone());
        Arc::new(move |at| {
            armed.lock().expect("lock").push(at);
            let clock = clock.clone();
            Box::pin(async move {
                while clock.now() < at {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
        })
    }

    async fn until(what: &str, mut cond: impl FnMut() -> bool) {
        let wait = async {
            while !cond() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .unwrap_or_else(|_| panic!("не дождались: {what}"));
    }

    /// Воркер под лизой на часах testkit.
    struct Rig {
        e: Env,
        gc: Arc<GcWorker>,
        armed: Arc<std::sync::Mutex<Vec<OffsetDateTime>>>,
        stop: tokio::sync::oneshot::Sender<()>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Rig {
        fn start(e: Env, cfg: GcConfig) -> Self {
            let armed = Arc::new(std::sync::Mutex::new(Vec::new()));
            let c = e.clock.clone();
            let gc = Arc::new(
                GcWorker::new(
                    Arc::new(e.index.clone()),
                    Arc::new(e.store.clone()),
                    GcConfig {
                        window: Duration::from_secs(W),
                        max_deletes_per_sec: 0,
                        ..cfg
                    },
                    Arc::new(move || c.now()),
                )
                .with_sleep(test_sleep(&e.clock, &armed)),
            );
            let leases = Arc::new(MemLease::new(e.clock.shared()));
            let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
            let w = gc.clone();
            let task = tokio::spawn(async move {
                w.run(&*leases, "node-a", async {
                    stopped.await.ok();
                })
                .await;
            });
            Self {
                e,
                gc,
                armed,
                stop,
                task,
            }
        }

        fn armed(&self) -> Vec<OffsetDateTime> {
            self.armed.lock().expect("lock").clone()
        }

        fn last(&self) -> Option<OffsetDateTime> {
            self.armed().last().copied()
        }

        async fn shutdown(self) {
            self.stop.send(()).ok();
            tokio::time::timeout(Duration::from_secs(5), self.task)
                .await
                .expect("воркер остановлен")
                .expect("задача");
        }
    }

    async fn zeroed(e: &Env, body: &'static str) -> (BlobKey, OffsetDateTime) {
        let key = put(e, body, 1).await;
        e.index
            .release(T, &key.hash, key.generation, 1)
            .await
            .expect("вызов");
        (key, e.clock.now())
    }

    fn secs(n: u64) -> time::Duration {
        time::Duration::try_from(Duration::from_secs(n)).expect("длительность")
    }

    /// Один таймер на ближайший срок: объект удаляется в срок, без периодических проходов.
    #[tokio::test]
    async fn timer_on_nearest_due() {
        let e = env(GcConfig::default());
        let (a, za) = zeroed(&e, "first").await;
        e.clock.advance(Duration::from_secs(100));
        let (b, zb) = zeroed(&e, "second").await;
        let r = Rig::start(
            e,
            GcConfig {
                max_wait: Duration::from_secs(100 * W),
                ..GcConfig::default()
            },
        );

        // Таймер — на срок первого: сразу после `zero_since + W`.
        until("таймер на срок первого", || {
            r.last()
                .is_some_and(|t| t > za + secs(W) && t <= za + secs(W + 1))
        })
        .await;
        let due_a = r.last().expect("срок");
        r.e.clock.set(due_a - secs(1));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(r.e.store.contains(&a));
        assert_eq!(r.gc.metrics().passes, 0);

        r.e.clock.set(due_a);
        until("первый удалён", || !r.e.store.contains(&a)).await;
        assert!(r.e.store.contains(&b));
        until("таймер на срок второго", || {
            r.last()
                .is_some_and(|t| t > zb + secs(W) && t <= zb + secs(W + 1))
        })
        .await;
        assert_eq!(r.gc.metrics().passes, 1);

        r.e.clock.set(r.last().expect("срок"));
        until("второй удалён", || !r.e.store.contains(&b)).await;
        // Кандидатов нет: таймер — только сверка не позже чем через `min(max_wait, window)`, без прохода.
        let now = r.e.clock.now();
        until("сверка через окно", || {
            r.last() == Some(now + secs(W))
        })
        .await;
        r.e.clock.advance(Duration::from_secs(W));
        until("сверка повторена", || {
            r.last() == Some(now + secs(2 * W))
        })
        .await;
        assert_eq!(r.gc.metrics().passes, 2);
        r.shutdown().await;
    }

    /// Сигнал `wake` — сверка срока без движения часов.
    #[tokio::test]
    async fn wake_rechecks_due() {
        let e = env(GcConfig::default());
        let r = Rig::start(
            e,
            GcConfig {
                max_wait: Duration::from_secs(60),
                ..GcConfig::default()
            },
        );
        let t0 = r.e.clock.now();
        until("сверка через max_wait", || {
            r.last() == Some(t0 + secs(60))
        })
        .await;
        let n = r.armed().len();
        r.gc.wake();
        until("перевзвод по сигналу", || {
            r.armed().len() > n
        })
        .await;
        assert_eq!(r.gc.metrics().passes, 0);
        r.shutdown().await;
    }

    /// Ошибка хранилища в проходе: повтор не раньше `retry`, а не немедленно.
    #[tokio::test]
    async fn error_retried_after_retry() {
        let e = env(GcConfig::default());
        let (key, _) = zeroed(&e, "flaky").await;
        e.clock.advance(Duration::from_secs(W + 1));
        e.store.fail_next(BlobOp::Delete, Error::UNAVAILABLE);
        let t0 = e.clock.now();
        let r = Rig::start(
            e,
            GcConfig {
                retry: Duration::from_secs(30),
                ..GcConfig::default()
            },
        );
        until("ошибка прохода", || r.gc.metrics().errors == 1).await;
        until("повтор через retry", || {
            r.last() == Some(t0 + secs(30))
        })
        .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(r.e.store.contains(&key));
        r.e.clock.advance(Duration::from_secs(30));
        until("удалён повтором", || {
            !r.e.store.contains(&key)
        })
        .await;
        r.shutdown().await;
    }
}
