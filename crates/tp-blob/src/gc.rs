//! GC объектов BLOB (SRS 3.4, 3.5): записи без ссылок старше окна W удаляются из хранилища.
//!
//! Проход: [`BlobIndex::gc_candidates`] пакетами → [`BlobIndex::mark_deleting`] (условие `refcnt = 0`
//! и окно W проверяются одной транзакцией) → [`BlobStore::delete`] → [`BlobIndex::finalize_delete`].
//! Окно W не меньше глубины самой старой точки восстановления (инвариант W). Работает один узел —
//! держатель лизы лидера; лиза продлевается между пакетами, потеря лизы прекращает проход.
//! Сбой удаления оставляет запись `deleting` — следующий проход повторит её.
//!
//! Воркер ([`GcWorker::run`]) не делает периодических проходов (SRS 2.8, таймер 4): один таймер на ближайший
//! срок окончания отсрочки — `zero_since + W` самой ранней записи без ссылок. Контракт учёта сроков
//! не отдаёт, поэтому срок находится делением пополам по [`BlobIndex::gc_candidates`] с пределом 1
//! (запрос по индексу `zero_since`) с точностью [`GcConfig::precision`]; таймер взводится на верхнюю
//! границу — объект удаляется не раньше срока и не позже чем через `precision` после него.
//! Ожидание не дольше `min(max_wait, window)`: по его истечении срок сверяется заново (без прохода).
//! Ссылки, обнулённые после взвода, дают срок не раньше `now + W`, то есть не раньше взведённого таймера, —
//! поэтому обнуление на любом узле без сигнала не опаздывает; сигнал [`GcWorker::wake`] после фиксации
//! обнуления в этом процессе перевзводит таймер сразу. Просроченное после прохода или ошибки — повтор
//! не раньше [`GcConfig::retry`] (таймер 2), лиза у другого узла — не раньше [`GcConfig::lease_ttl`] (таймер 1).

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::Notify;
use tp_contracts::Result;
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::config::duration;
use tp_contracts::lease::{Lease, LeaseStore};

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
/// precision: 1m
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
    /// Размер пакета кандидатов.
    pub batch: usize,
    /// Предел удалений в секунду; 0 — без ограничения.
    pub max_deletes_per_sec: u32,
    /// Наибольшее ожидание таймера (не больше `window`): по истечении ближайший срок сверяется заново.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub max_wait: Duration,
    /// Точность поиска ближайшего срока: удаление не позже чем через столько после срока.
    /// Меньше — точнее, но больше запросов к учёту на один взвод (log2(max_wait / precision)).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub precision: Duration,
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
            precision: Duration::from_secs(60),
            retry: Duration::from_secs(300),
            lease_name: "blob-gc".to_owned(),
            lease_ttl: Duration::from_secs(120),
        }
    }
}

/// Итог одного прохода.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcPass {
    /// Получено кандидатов.
    pub candidates: u64,
    /// Удалено объектов (запись заменена надгробием или уже другого поколения).
    pub deleted: u64,
    /// Кандидатов, которых удалять нельзя: ссылки вернулись или поколение сменилось.
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
    /// Срок записи — `zero_since + W`: при `zero_since < t` проход в момент `t + W` её удаляет.
    async fn next_due(
        &self,
        now: OffsetDateTime,
        cap: OffsetDateTime,
    ) -> Result<Option<OffsetDateTime>> {
        let w = self.cfg.window;
        let any = |t: OffsetDateTime| async move {
            Ok::<_, tp_contracts::Error>(!self.index.gc_candidates(t, 1).await?.is_empty())
        };
        let mut lo = before(now, w);
        if any(lo).await? {
            return Ok(Some(now));
        }
        let mut hi = before(cap, w);
        if hi <= lo || !any(hi).await? {
            return Ok(None);
        }
        // Нет записей с `zero_since < lo`, есть с `zero_since < hi`.
        let step = time::Duration::try_from(self.cfg.precision.max(Duration::from_millis(1)))
            .unwrap_or(time::Duration::SECOND);
        while hi - lo > step {
            let mid = lo + (hi - lo) / 2;
            if any(mid).await? {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        Ok(Some(after(hi, w)))
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
        // Граница окна фиксируется на проход: кандидаты и `mark_deleting` проверяют одно условие.
        let older_than = before((self.now)(), self.cfg.window);
        let batch = self.cfg.batch.max(1);
        let mut pacer = Pacer::new(self.cfg.max_deletes_per_sec);
        let mut pass = GcPass::default();
        loop {
            let keys = self.index.gc_candidates(older_than, batch).await?;
            pass.candidates += keys.len() as u64;
            let mut progress = false;
            for key in &keys {
                inc(&self.counters.candidates);
                if !self.index.mark_deleting(key, older_than).await? {
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
            // Неполный пакет — кандидаты кончились. Пакет без продвижения (ни одна запись не заменена
            // надгробием и не пропущена) — защита от зацикливания на одних и тех же кандидатах.
            if keys.len() < batch || !progress {
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
    use tp_contracts::blob::Acquire;
    use tp_contracts::{BlobKey, Error, Gen, TenantId};
    use tp_testkit::blob::{BlobOp, BlobState, MemBlobIndex, MemBlobStore};
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

    /// Один таймер на ближайший срок: объект удаляется в срок (с точностью `precision`),
    /// без периодических проходов.
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
                precision: Duration::from_secs(1),
                ..GcConfig::default()
            },
        );

        // Таймер — на срок первого: не раньше `zero_since + W`, не позже на `precision`.
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
