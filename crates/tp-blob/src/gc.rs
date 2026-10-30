//! GC объектов BLOB (SRS 3.4, 3.5): записи без ссылок старше окна W удаляются из хранилища.
//!
//! Проход: [`BlobIndex::gc_candidates`] пакетами → [`BlobIndex::mark_deleting`] (условие `refcnt = 0`
//! и окно W проверяются одной транзакцией) → [`BlobStore::delete`] → [`BlobIndex::finalize_delete`].
//! Окно W не меньше глубины самой старой точки восстановления (инвариант W). Работает один узел —
//! держатель лизы лидера; лиза продлевается между пакетами, потеря лизы прекращает проход.
//! Сбой удаления оставляет запись `deleting` — следующий проход повторит её.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use schemars::JsonSchema;
use tp_contracts::Result;
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::lease::LeaseStore;

use crate::worker::{Leader, Now, Pacer, Schedule, before, keep, run_leased};

/// Конфигурация GC.
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
pub struct GcConfig {
    /// Окно W: объект удаляется не раньше, чем через столько секунд после обнуления ссылок.
    /// Не меньше глубины самой старой точки восстановления (PITR, DR-реплика, снапшоты).
    pub window_secs: u64,
    /// Размер пакета кандидатов.
    pub batch: usize,
    /// Предел удалений в секунду; 0 — без ограничения.
    pub max_deletes_per_sec: u32,
    /// Пауза между проходами, с.
    pub interval_secs: u64,
    /// Имя лизы лидера GC.
    pub lease_name: String,
    /// Срок лизы, с; продлевается между пакетами.
    pub lease_ttl_secs: u64,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            window_secs: 7 * 24 * 3600,
            batch: 1000,
            max_deletes_per_sec: 200,
            interval_secs: 300,
            lease_name: "blob-gc".to_owned(),
            lease_ttl_secs: 120,
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
        Self {
            index,
            store,
            cfg,
            now,
            counters: Counters::default(),
        }
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

    /// Периодические проходы под лизой лидера до `shutdown`; `holder` — идентификатор узла.
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
            ttl: Duration::from_secs(self.cfg.lease_ttl_secs),
            interval: Duration::from_secs(self.cfg.interval_secs),
            what: "blob-gc",
        };
        run_leased(s, shutdown, async |leader: &mut Leader<'_>| {
            self.pass(Some(leader)).await.map(drop)
        })
        .await;
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
        let older_than = before((self.now)(), self.cfg.window_secs);
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
        gc: GcWorker,
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
        let gc = GcWorker::new(
            Arc::new(index.clone()),
            Arc::new(store.clone()),
            GcConfig {
                window_secs: W,
                max_deletes_per_sec: 0,
                ..cfg
            },
            Arc::new(move || c.now()),
        );
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
            e.index.acquire(T, &key.hash, 1).await.expect("вызов"),
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
        let e = env(GcConfig {
            interval_secs: 3600,
            ..GcConfig::default()
        });
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
}
