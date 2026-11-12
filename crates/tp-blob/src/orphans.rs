//! Поиск «сирот» (SRS 3.4): объекты хранилища старше периода ожидания без записи учёта
//! с совпадающими хэшем и поколением удаляются.
//!
//! «Сироты» появляются при обрыве записи между загрузкой и `commit_new` и при параллельной записи
//! объекта другого поколения. Период ожидания больше максимальной длительности загрузки, поэтому
//! объект незавершённой записи не трогается; ключ, по которому уже лежит объект, писатель повторно
//! не занимает (`writer`), поэтому устаревший листинг не задевает новую запись.
//! Пути, не являющиеся ключами BLOB (сегменты индексов
//! `put_raw`), пропускаются. Работа низкого приоритета: проверки и удаления ограничены по скорости,
//! один узел — держатель лизы. Проходы — по расписанию из конфигурации ([`OrphanConfig::interval`];
//! SRS 2.8, закрытый список таймеров, п. 4).
//!
//! Метрики (критерий Д.7 «"Сироты" после цикла GC — 0», решение 57): найдено, удалено и осталось после
//! прохода — [`OrphanMetrics`], описания для `/metrics` — [`OrphanMetrics::STATS`]. «Осталось» — найденные
//! «сироты», удаление которых не подтверждено: после удаления объект проверяется `head`; при обрыве прохода
//! неудалённая «сирота» остаётся в счёте.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::blob::{BlobIndex, BlobStore, ObjectEntry};
use tp_contracts::config::duration;
use tp_contracts::lease::LeaseStore;
use tp_contracts::{BlobKey, Result};

use crate::worker::{Leader, LeasedPass, Now, Pacer, Schedule, before, keep, run_leased};

/// Конфигурация поиска «сирот» (секция config.yaml; поля необязательны, неизвестное — ошибка).
///
/// ```yaml
/// grace: 24h
/// prefix: ""
/// max_checks_per_sec: 500
/// max_deletes_per_sec: 50
/// renew_every: 1000
/// interval: 6h
/// lease_name: blob-orphans
/// lease_ttl: 2m
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OrphanConfig {
    /// Период ожидания: объекты моложе не проверяются. Больше максимальной длительности загрузки.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub grace: Duration,
    /// Префикс путей листинга (`""` — всё хранилище, `"{tenant}/"` — одна организация).
    pub prefix: String,
    /// Предел проверок записей учёта в секунду; 0 — без ограничения.
    pub max_checks_per_sec: u32,
    /// Предел удалений в секунду; 0 — без ограничения.
    pub max_deletes_per_sec: u32,
    /// Число проверенных объектов между продлениями лизы.
    pub renew_every: u64,
    /// Расписание: пауза между проходами (SRS 2.8, таймер 4 — расписание фоновых работ из конфигурации).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub interval: Duration,
    /// Имя лизы.
    pub lease_name: String,
    /// Срок лизы.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub lease_ttl: Duration,
}

impl Default for OrphanConfig {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(24 * 3600),
            prefix: String::new(),
            max_checks_per_sec: 500,
            max_deletes_per_sec: 50,
            renew_every: 1000,
            interval: Duration::from_secs(6 * 3600),
            lease_name: "blob-orphans".to_owned(),
            lease_ttl: Duration::from_secs(120),
        }
    }
}

/// Итог одного прохода.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OrphanPass {
    /// Объектов в листинге (старше периода ожидания).
    pub listed: u64,
    /// Проверено ключей BLOB.
    pub checked: u64,
    /// Найдено «сирот».
    pub found: u64,
    /// Удалено «сирот».
    pub deleted: u64,
    /// Найденных «сирот», которых после прохода нет оснований считать удалёнными: удаление не выполнено
    /// (проход оборван) или объект после удаления остался в хранилище.
    pub remaining: u64,
    /// Проход прекращён: лиза потеряна.
    pub lease_lost: bool,
}

/// Снимок метрик поиска «сирот»: накопительные счётчики и `remaining` — значение последнего прохода.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OrphanMetrics {
    /// Завершённых проходов.
    pub passes: u64,
    /// Проверено ключей.
    pub checked: u64,
    /// Найдено «сирот».
    pub found: u64,
    /// Удалено «сирот».
    pub deleted: u64,
    /// «Сирот» осталось после последнего прохода ([`OrphanPass::remaining`]; критерий Д.7 — 0).
    /// Полный проход записывает своё значение, оборванный (ошибка, потеря лизы) — только ненулевое:
    /// ноль по части листинга ничего не доказывает. До первого полного прохода — 0 при `passes = 0`.
    pub remaining: u64,
    /// Ошибок.
    pub errors: u64,
}

/// Описание показателя для `/metrics`: имя, пояснение, значение из снимка, счётчик (иначе — текущее значение).
pub type OrphanStat = (&'static str, &'static str, fn(&OrphanMetrics) -> u64, bool);

impl OrphanMetrics {
    /// Показатели «сирот» для отчёта по Д.7: `tp_blob_orphans_{passes,found,deleted}_total`,
    /// `tp_blob_orphans_remaining`. Регистрирует приложение — снимок [`OrphanSweeper::metrics`]
    /// читается на каждый запрос `/metrics`.
    pub const STATS: [OrphanStat; 4] = [
        (
            "tp_blob_orphans_passes",
            "Завершённые проходы поиска «сирот» BLOB",
            |m| m.passes,
            true,
        ),
        (
            "tp_blob_orphans_found",
            "Найдено «сирот» BLOB",
            |m| m.found,
            true,
        ),
        (
            "tp_blob_orphans_deleted",
            "Удалено «сирот» BLOB",
            |m| m.deleted,
            true,
        ),
        (
            "tp_blob_orphans_remaining",
            "«Сирот» BLOB осталось после последнего прохода",
            |m| m.remaining,
            false,
        ),
    ];
}

#[derive(Debug, Default)]
struct Counters {
    passes: AtomicU64,
    checked: AtomicU64,
    found: AtomicU64,
    deleted: AtomicU64,
    remaining: AtomicU64,
    errors: AtomicU64,
}

fn inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Ключ объекта листинга, если это «сирота»: ключ BLOB без записи учёта того же поколения.
pub(crate) async fn orphan_key(
    index: &dyn BlobIndex,
    entry: &ObjectEntry,
) -> Result<Option<BlobKey>> {
    let Some(key) = entry.key() else {
        return Ok(None);
    };
    Ok((!index.exists(&key).await?).then_some(key))
}

/// Воркер поиска «сирот».
pub struct OrphanSweeper {
    index: Arc<dyn BlobIndex>,
    store: Arc<dyn BlobStore>,
    cfg: OrphanConfig,
    now: Now,
    counters: Counters,
}

impl std::fmt::Debug for OrphanSweeper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrphanSweeper")
            .field("cfg", &self.cfg)
            .field("metrics", &self.metrics())
            .finish_non_exhaustive()
    }
}

impl OrphanSweeper {
    /// Воркер поверх учёта `index` и хранилища `store`; `now` — часы UTC ([`crate::system_now`]).
    pub fn new(
        index: Arc<dyn BlobIndex>,
        store: Arc<dyn BlobStore>,
        cfg: OrphanConfig,
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
    pub fn metrics(&self) -> OrphanMetrics {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        OrphanMetrics {
            passes: get(&c.passes),
            checked: get(&c.checked),
            found: get(&c.found),
            deleted: get(&c.deleted),
            remaining: get(&c.remaining),
            errors: get(&c.errors),
        }
    }

    /// Разовый проход без лизы.
    pub async fn run_once(&self) -> Result<OrphanPass> {
        self.pass(None).await
    }

    /// Проходы под лизой по расписанию [`OrphanConfig::interval`] до `shutdown`; `holder` — идентификатор узла.
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
            what: "blob-orphans",
        };
        run_leased(s, self.cfg.interval, shutdown, self).await;
    }

    async fn pass(&self, mut leader: Option<&mut Leader<'_>>) -> Result<OrphanPass> {
        let mut p = OrphanPass::default();
        let res = self.pass_inner(&mut leader, &mut p).await;
        // Полный проход — его значение; оборванный — только известный остаток.
        if p.remaining > 0 || (res.is_ok() && !p.lease_lost) {
            self.counters
                .remaining
                .store(p.remaining, Ordering::Relaxed);
        }
        match res {
            Ok(()) => {
                inc(&self.counters.passes);
                tracing::info!(
                    listed = p.listed,
                    checked = p.checked,
                    found = p.found,
                    deleted = p.deleted,
                    remaining = p.remaining,
                    "проход поиска «сирот» BLOB"
                );
                Ok(p)
            }
            Err(e) => {
                inc(&self.counters.errors);
                tracing::warn!(error = %e, remaining = p.remaining, "поиск «сирот» BLOB прерван");
                Err(e)
            }
        }
    }

    async fn pass_inner(
        &self,
        leader: &mut Option<&mut Leader<'_>>,
        pass: &mut OrphanPass,
    ) -> Result<()> {
        let older_than = before((self.now)(), self.cfg.grace);
        let mut checks = Pacer::new(self.cfg.max_checks_per_sec);
        let mut deletes = Pacer::new(self.cfg.max_deletes_per_sec);
        let renew_every = self.cfg.renew_every.max(1);
        let mut list = self.store.list(&self.cfg.prefix, older_than);
        while let Some(entry) = list.next().await {
            let entry = entry?;
            pass.listed += 1;
            // Низкий приоритет: уступить исполнитель почтовым задачам.
            tokio::task::yield_now().await;
            if entry.key().is_none() {
                continue;
            }
            checks.wait().await;
            pass.checked += 1;
            inc(&self.counters.checked);
            if let Some(key) = orphan_key(self.index.as_ref(), &entry).await? {
                pass.found += 1;
                pass.remaining += 1;
                inc(&self.counters.found);
                deletes.wait().await;
                self.store.delete(&key).await?;
                pass.deleted += 1;
                inc(&self.counters.deleted);
                // Удаление подтверждается: объект, оставшийся после `delete`, — «сирота» после прохода.
                if self.store.head(&key).await?.is_none() {
                    pass.remaining -= 1;
                    tracing::debug!(%key, "«сирота» BLOB удалена");
                } else {
                    tracing::warn!(%key, "«сирота» BLOB осталась после удаления");
                }
            }
            if pass.checked.is_multiple_of(renew_every) && !keep(leader).await? {
                pass.lease_lost = true;
                break;
            }
        }
        Ok(())
    }
}

impl LeasedPass for OrphanSweeper {
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
    use tp_contracts::{BlobHash, Error, Gen, TenantId};
    use tp_testkit::blob::{BlobOp, MemBlobIndex, MemBlobStore};
    use tp_testkit::{Clock, ManualClock, MemLease};

    use crate::writer::{BlobHasher, BlobWriter, WriterConfig};

    const T: TenantId = TenantId::DEFAULT;
    const GRACE: u64 = 3600;

    struct Env {
        clock: ManualClock,
        index: MemBlobIndex,
        store: MemBlobStore,
        writer: BlobWriter,
        sweeper: Arc<OrphanSweeper>,
    }

    fn env() -> Env {
        let clock = ManualClock::new(OffsetDateTime::UNIX_EPOCH + time::Duration::days(1000));
        let index = MemBlobIndex::new(clock.shared());
        let store = MemBlobStore::new(clock.shared());
        let writer = BlobWriter::new(
            Arc::new(index.clone()),
            Arc::new(store.clone()),
            WriterConfig::default(),
        );
        let c = clock.clone();
        let sweeper = Arc::new(OrphanSweeper::new(
            Arc::new(index.clone()),
            Arc::new(store.clone()),
            OrphanConfig {
                grace: Duration::from_secs(GRACE),
                max_checks_per_sec: 0,
                max_deletes_per_sec: 0,
                ..OrphanConfig::default()
            },
            Arc::new(move || c.now()),
        ));
        Env {
            clock,
            index,
            store,
            writer,
            sweeper,
        }
    }

    /// Запись, оборванная после загрузки до `commit_new`.
    async fn interrupted(e: &Env, body: &'static [u8]) -> BlobKey {
        let hash: BlobHash = BlobHasher::digest(body);
        let Acquire::Absent { generation } =
            e.index.acquire(T, None, &hash, 1).await.expect("вызов")
        else {
            panic!("объект уже есть");
        };
        let key = BlobKey {
            tenant: T,
            hash,
            generation,
        };
        e.store
            .put(&key, Bytes::from_static(body))
            .await
            .expect("вызов");
        key
    }

    #[tokio::test]
    async fn interrupted_commit_removed() {
        let e = env();
        let orphan = interrupted(&e, b"lost").await;
        let live = e
            .writer
            .write(T, Bytes::from_static(b"live"), 1)
            .await
            .expect("вызов");
        e.store
            .put_raw("idx/t/a/seg", Bytes::from_static(b"segment"))
            .await
            .expect("вызов");

        e.clock.advance(Duration::from_secs(GRACE + 1));
        let pass = e.sweeper.run_once().await.expect("вызов");
        assert_eq!((pass.listed, pass.checked, pass.deleted), (3, 2, 1));
        assert!(!e.store.contains(&orphan));
        assert!(e.store.contains(&live));
        assert!(e.store.bytes("idx/t/a/seg").is_some());
        assert_eq!(e.sweeper.metrics().deleted, 1);
    }

    /// Приёмка BLOB-16 (Д.7): после прохода с оборванной загрузкой «сирот» 0, метрика это показывает.
    #[tokio::test]
    async fn metrics_show_no_orphans_after_pass() {
        let e = env();
        let orphan = interrupted(&e, b"lost").await;
        e.writer
            .write(T, Bytes::from_static(b"live"), 1)
            .await
            .expect("вызов");
        e.clock.advance(Duration::from_secs(GRACE + 1));

        let pass = e.sweeper.run_once().await.expect("вызов");
        assert_eq!((pass.found, pass.deleted, pass.remaining), (1, 1, 0));
        assert!(!e.store.contains(&orphan));
        let m = e.sweeper.metrics();
        assert_eq!((m.passes, m.found, m.deleted, m.remaining), (1, 1, 1, 0));

        // Повторный проход «сирот» не находит: счётчики не растут, остаток — 0.
        let pass = e.sweeper.run_once().await.expect("вызов");
        assert_eq!((pass.found, pass.remaining), (0, 0));
        let m = e.sweeper.metrics();
        assert_eq!((m.passes, m.found, m.deleted, m.remaining), (2, 1, 1, 0));

        // Показатели для `/metrics` читают тот же снимок.
        let stats: Vec<_> = OrphanMetrics::STATS
            .iter()
            .map(|(name, _, pick, counter)| (*name, pick(&m), *counter))
            .collect();
        assert_eq!(
            stats,
            [
                ("tp_blob_orphans_passes", 2, true),
                ("tp_blob_orphans_found", 1, true),
                ("tp_blob_orphans_deleted", 1, true),
                ("tp_blob_orphans_remaining", 0, false),
            ]
        );
    }

    /// Сбой удаления: «сирота» остаётся в метрике до прохода, который её удалит.
    #[tokio::test]
    async fn failed_delete_counts_as_remaining() {
        let e = env();
        let orphan = interrupted(&e, b"stuck").await;
        e.clock.advance(Duration::from_secs(GRACE + 1));

        e.store.fail_next(BlobOp::Delete, Error::UNAVAILABLE);
        assert_eq!(e.sweeper.run_once().await, Err(Error::UNAVAILABLE));
        assert!(e.store.contains(&orphan));
        let m = e.sweeper.metrics();
        assert_eq!((m.found, m.deleted, m.remaining, m.errors), (1, 0, 1, 1));

        // Оборванный проход без известного остатка значение не сбрасывает.
        e.store.fail_next(BlobOp::List, Error::UNAVAILABLE);
        assert_eq!(e.sweeper.run_once().await, Err(Error::UNAVAILABLE));
        assert_eq!(e.sweeper.metrics().remaining, 1);

        let pass = e.sweeper.run_once().await.expect("вызов");
        assert_eq!((pass.found, pass.deleted, pass.remaining), (1, 1, 0));
        let m = e.sweeper.metrics();
        assert_eq!((m.found, m.deleted, m.remaining), (2, 1, 0));
    }

    #[tokio::test]
    async fn young_object_untouched() {
        let e = env();
        let old = interrupted(&e, b"old").await;
        e.clock.advance(Duration::from_secs(GRACE + 1));
        // Загрузка ещё идёт: объект моложе периода ожидания.
        let young = interrupted(&e, b"young").await;
        let pass = e.sweeper.run_once().await.expect("вызов");
        assert_eq!(pass.deleted, 1);
        assert!(!e.store.contains(&old));
        assert!(e.store.contains(&young));

        // Запись завершилась — объект больше не «сирота».
        e.index
            .commit_new(T, &young.hash, young.generation, 5, 1)
            .await
            .expect("вызов");
        e.clock.advance(Duration::from_secs(GRACE + 1));
        assert_eq!(e.sweeper.run_once().await.expect("вызов").deleted, 0);
        assert!(e.store.contains(&young));
    }

    #[tokio::test]
    async fn stale_generation_removed() {
        let e = env();
        let live = e
            .writer
            .write(T, Bytes::from_static(b"gen"), 1)
            .await
            .expect("вызов");
        // Объект поколения, которого нет в учёте (параллельная запись другого поколения).
        let stale = BlobKey {
            generation: Gen(5),
            ..live
        };
        e.store
            .put(&stale, Bytes::from_static(b"gen"))
            .await
            .expect("вызов");
        e.clock.advance(Duration::from_secs(GRACE + 1));
        assert_eq!(e.sweeper.run_once().await.expect("вызов").deleted, 1);
        assert!(!e.store.contains(&stale));
        assert!(e.store.contains(&live));
    }

    #[tokio::test]
    async fn list_error_stops_pass() {
        let e = env();
        e.store.fail_next(BlobOp::List, Error::UNAVAILABLE);
        assert_eq!(e.sweeper.run_once().await, Err(Error::UNAVAILABLE));
        assert_eq!(e.sweeper.metrics().errors, 1);
    }

    /// Фоновая работа: `run` — `Send`-футура, задача tokio выполняет проход под лизой и останавливается.
    #[tokio::test]
    async fn run_in_spawned_task() {
        let e = env();
        let orphan = interrupted(&e, b"spawned").await;
        e.clock.advance(Duration::from_secs(GRACE + 1));

        let leases = Arc::new(MemLease::new(e.clock.shared()));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let (sweeper, l) = (e.sweeper.clone(), leases.clone());
        let task = tokio::spawn(async move {
            sweeper
                .run(&*l, "node-a", async {
                    rx.await.ok();
                })
                .await;
        });
        for _ in 0..100 {
            if !e.store.contains(&orphan) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tx.send(()).ok();
        task.await.expect("задача");
        assert!(!e.store.contains(&orphan));
        assert_eq!(leases.holder("blob-orphans"), None);
    }
}
