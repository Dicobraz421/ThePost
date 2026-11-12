//! Сервисы ролей ядра (SRS 2.1): maintenance — GC и «сироты» BLOB под лизой лидера, истечение срока корзины
//! (`tp-mailstore`: таймер на ближайший срок → точка принятия решений → окончательное удаление), подписчики
//! журнала событий (по сигналам, [`crate::log_reader`]), очистка по сроку надгробий DAV и журнала автоответов
//! ([`Purge`], решение 2026-11-11 п. 65); роли протоколов, ещё не реализованные в этой сборке, — заглушки, готовые сразу (Ф0).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tp_blob::{GcConfig, GcWorker, OrphanConfig, OrphanMetrics, OrphanSweeper, system_now};
use tp_contracts::Result;
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::events::EventSink;
use tp_contracts::lease::LeaseStore;
use tp_contracts::role::{CancellationToken, Role, RoleCtx, RoleService};
use tp_meta::PgMeta;
use tp_meta::events::EventConsumer;

use crate::log_reader::{LogReader, LogSignals};

/// Роль ядра, реализация которой ещё не вошла в сборку (протоколы — этап Ф1): объявляет готовность
/// и ждёт остановки, чтобы узел с пресетом `all` доходил до ready.
#[derive(Debug, Clone, Copy)]
pub struct PendingRole(pub Role);

#[async_trait]
impl RoleService for PendingRole {
    fn name(&self) -> &str {
        self.0.as_str()
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        tracing::warn!(role = %self.0, "роль ещё не реализована в этой сборке: соединений не принимает");
        ctx.ready.set(true);
        ctx.shutdown.cancelled().await;
        Ok(())
    }
}

/// Воркер поиска «сирот» роли maintenance для `/metrics` (APP-34): создаётся при первом запуске роли и
/// переживает её перезапуски — счётчики не обнуляются. До запуска роли снимок нулевой. Клон — тот же воркер.
#[derive(Clone, Default)]
pub struct OrphanProbe(Arc<OnceLock<Arc<OrphanSweeper>>>);

impl OrphanProbe {
    /// Снимок метрик воркера ([`OrphanSweeper::metrics`]).
    pub fn metrics(&self) -> OrphanMetrics {
        self.0.get().map(|s| s.metrics()).unwrap_or_default()
    }
}

impl std::fmt::Debug for OrphanProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrphanProbe")
            .field("started", &self.0.get().is_some())
            .finish()
    }
}

/// Зависимости роли maintenance.
pub struct Maintenance {
    /// Учёт объектов BLOB.
    pub index: Arc<dyn BlobIndex>,
    /// Хранилище BLOB.
    pub store: Arc<dyn BlobStore>,
    /// Лизы лидера GC и поиска «сирот».
    pub leases: Arc<dyn LeaseStore>,
    /// Держатель лиз: узел и процесс.
    pub holder: String,
    /// Обработчик журнала событий с назначением разделов узлу.
    pub events: EventConsumer,
    /// Подписчики журнала событий (модули).
    pub sinks: Vec<Arc<dyn EventSink>>,
    /// Сигналы, будящие подписчиков журнала (опроса нет).
    pub signals: LogSignals,
    /// Сборка мусора объектов (`blob.gc`).
    pub gc: GcConfig,
    /// Поиск «сирот» (`blob.orphans`).
    pub orphans: OrphanConfig,
    /// Воркер поиска «сирот»: его метрики читает `/metrics` (`crate::metrics::register_orphans`).
    pub orphan_probe: OrphanProbe,
    /// Почтовое хранилище для истечения срока корзины (`TrashStore::expired` → `before_final_delete` →
    /// `expunge_final`, секция `mailstore.trash`); `None` — истечение не выполняется (тесты).
    pub trash: Option<tp_mailstore::Store>,
    /// Очистка по сроку надгробий DAV и журнала автоответов; `None` — не выполняется (тесты без PostgreSQL).
    pub purge: Option<Purge>,
}

/// Очистка по сроку в роли maintenance (APP-32): надгробия DAV старше `meta.dav.tombstone_ttl` и записи журнала
/// автоответов старше `meta.sieve.vacation_log_ttl` — сроки знает [`PgMeta`]. Удаление по сроку идемпотентно,
/// повтор удаляет только оставшееся: как и истечение корзины, работает на каждом узле с ролью без лизы.
#[derive(Clone)]
pub struct Purge {
    /// Метаданные: шаги `purge_dav_tombstones` и `purge_vacation_log`.
    pub meta: PgMeta,
    /// Период (`maintenance.purge_interval`); первый проход — при запуске роли.
    pub interval: Duration,
}

/// Итог одного прохода [`Purge::step`]: число удалённых записей каждого шага.
#[derive(Debug)]
pub struct Purged {
    /// Надгробия DAV.
    pub dav_tombstones: Result<u64>,
    /// Записи журнала автоответов.
    pub vacation_log: Result<u64>,
}

impl std::fmt::Debug for Purge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Purge")
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

impl Purge {
    /// Один проход: оба шага независимы — ошибка одного не отменяет другой и ждёт следующего прохода.
    pub async fn step(&self) -> Purged {
        let done = Purged {
            dav_tombstones: self.meta.purge_dav_tombstones().await,
            vacation_log: self.meta.purge_vacation_log().await,
        };
        for (step, result) in [
            ("purge_dav_tombstones", &done.dav_tombstones),
            ("purge_vacation_log", &done.vacation_log),
        ] {
            match result {
                Ok(0) => tracing::debug!(step, "очистка по сроку: удалять нечего"),
                Ok(deleted) => tracing::info!(step, deleted, "очистка по сроку"),
                Err(e) => {
                    tracing::warn!(step, error = %e, "очистка по сроку не выполнена: повтор в следующий проход");
                }
            }
        }
        done
    }

    /// Проходы с периодом [`Purge::interval`] до остановки; остановка прерывает и идущий проход (незавершённая
    /// транзакция откатывается, следующий запуск удалит оставшееся).
    pub async fn run(self, shutdown: CancellationToken) {
        let mut tick = tokio::time::interval(self.interval.max(Duration::from_millis(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = tick.tick() => {}
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = self.step() => {}
            }
        }
    }
}

impl std::fmt::Debug for Maintenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Maintenance")
            .field("holder", &self.holder)
            .field("trash", &self.trash.is_some())
            .field("purge", &self.purge)
            .field(
                "sinks",
                &self.sinks.iter().map(|s| s.name()).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl RoleService for Maintenance {
    fn name(&self) -> &str {
        Role::Maintenance.as_str()
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        // Ленивая инициализация (SRS 2.7): воркеры создаются при запуске роли.
        let gc = GcWorker::new(
            self.index.clone(),
            self.store.clone(),
            self.gc.clone(),
            system_now(),
        );
        let orphans = self
            .orphan_probe
            .0
            .get_or_init(|| {
                Arc::new(OrphanSweeper::new(
                    self.index.clone(),
                    self.store.clone(),
                    self.orphans.clone(),
                    system_now(),
                ))
            })
            .clone();
        let (leases, holder, token) = (
            self.leases.clone(),
            self.holder.clone(),
            ctx.shutdown.clone(),
        );
        let gc = tokio::spawn(async move {
            gc.run(&*leases, &holder, token.cancelled_owned()).await;
        });
        let (leases, holder, token) = (
            self.leases.clone(),
            self.holder.clone(),
            ctx.shutdown.clone(),
        );
        let orphans = tokio::spawn(async move {
            orphans
                .run(&*leases, &holder, token.cancelled_owned())
                .await;
        });
        // Окончательное удаление необратимо, повтор безопасен (`expunge_final` пропускает удалённое): воркер
        // работает на каждом узле с ролью maintenance без лизы.
        let trash = self.trash.clone().map(|store| {
            let token = ctx.shutdown.clone();
            tokio::spawn(async move { store.run_trash_expiry(token.cancelled_owned()).await })
        });
        let purge = self
            .purge
            .clone()
            .map(|purge| tokio::spawn(purge.run(ctx.shutdown.clone())));
        ctx.ready.set(true);
        let trash = async move {
            match trash {
                Some(task) => task.await,
                None => Ok(()),
            }
        };
        let purge = async move {
            match purge {
                Some(task) => task.await,
                None => Ok(()),
            }
        };
        let (gc, orphans, trash, purge, ()) = tokio::join!(
            gc,
            orphans,
            trash,
            purge,
            consume(
                self.events.clone(),
                self.sinks.clone(),
                self.signals.clone(),
                ctx.shutdown.clone(),
            ),
        );
        for (what, done) in [
            ("blob-gc", gc),
            ("blob-orphans", orphans),
            ("trash-expiry", trash),
            ("purge", purge),
        ] {
            if let Err(e) = done {
                return Err(tp_contracts::Error::Internal(format!("{what}: {e}")));
            }
        }
        Ok(())
    }
}

/// Чтение журнала событий подписчиками по сигналам до остановки; затем лизы разделов отдаются другим узлам.
async fn consume(
    events: EventConsumer,
    sinks: Vec<Arc<dyn EventSink>>,
    signals: LogSignals,
    shutdown: CancellationToken,
) {
    if sinks.is_empty() {
        return;
    }
    let mut reader = LogReader::new(events, signals);
    while let Some(work) = reader.wait(&shutdown).await {
        for sink in &sinks {
            if let Err(e) = reader.read(sink.as_ref(), &work).await {
                tracing::warn!(subscriber = sink.name(), error = %e, "журнал событий: пачка не обработана");
            }
        }
    }
    for sink in &sinks {
        reader.release(sink.as_ref()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tp_contracts::events::{CoreEvent, CoreEventKind, EventLog, LoggedEvent};
    use tp_contracts::role::{ReadyHandle, fixed_config};
    use tp_contracts::{AccountId, TenantId};
    use tp_testkit::{Clock, MemBlobIndex, MemBlobStore, MemEventLog, MemLease, SystemClock};

    #[derive(Default)]
    struct Counting(AtomicUsize);

    #[async_trait]
    impl EventSink for Counting {
        fn name(&self) -> &str {
            "counting"
        }

        async fn handle(&self, batch: &[LoggedEvent]) -> Result<()> {
            self.0.fetch_add(batch.len(), Ordering::SeqCst);
            Ok(())
        }
    }

    fn ctx() -> (RoleCtx, CancellationToken, ReadyHandle) {
        let shutdown = CancellationToken::new();
        let ready = ReadyHandle::default();
        let ctx = RoleCtx {
            config: fixed_config(()),
            shutdown: shutdown.clone(),
            drain_deadline: Duration::from_secs(5),
            ready: ready.clone(),
            load: tp_contracts::load::fixed_load(tp_contracts::load::LoadLevel::Normal),
        };
        (ctx, shutdown, ready)
    }

    #[tokio::test]
    async fn pending_role_is_ready_until_shutdown() {
        let (ctx, shutdown, ready) = ctx();
        let task = tokio::spawn(async move { PendingRole(Role::Imap).run(ctx).await });
        while !ready.is_ready() {
            tokio::task::yield_now().await;
        }
        shutdown.cancel();
        task.await.expect("join").expect("run");
        assert_eq!(PendingRole(Role::Imap).name(), "imap");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn maintenance_consumes_events_and_stops() {
        let log = MemEventLog::new(4);
        let leases = MemLease::default();
        let sink = Arc::new(Counting::default());
        log.append(CoreEvent {
            tenant: TenantId::DEFAULT,
            account: AccountId::new(),
            mailbox: None,
            kind: CoreEventKind::MailboxCreated,
            at: SystemClock.now(),
        });
        let events = EventConsumer::new(
            Arc::new(log.clone()) as Arc<dyn EventLog>,
            Arc::new(leases.clone()),
            "node-a/1",
            tp_meta::events::EventsConfig::default(),
        );
        let role = Maintenance {
            index: Arc::new(MemBlobIndex::default()),
            store: Arc::new(MemBlobStore::default()),
            leases: Arc::new(leases),
            holder: "node-a/1".into(),
            events,
            sinks: vec![sink.clone()],
            signals: LogSignals {
                bus: Arc::new(tp_cluster::LocalBus::new(
                    &tp_cluster::LocalBusConfig::default(),
                )),
                members: None,
                lease_ttl: Duration::from_secs(30),
                resubscribe: Duration::from_secs(1),
            },
            gc: GcConfig::default(),
            orphans: OrphanConfig::default(),
            orphan_probe: OrphanProbe::default(),
            trash: None,
            purge: None,
        };
        let (ctx, shutdown, ready) = ctx();
        let task = tokio::spawn(async move { role.run(ctx).await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while sink.0.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "событие не обработано"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready.is_ready());
        shutdown.cancel();
        task.await.expect("join").expect("run");
    }

    /// APP-32: проход очистки удаляет надгробия DAV и записи журнала автоответов старше срока из `meta`, свежие
    /// остаются; роль maintenance выполняет проход при запуске. Отдельная база: очистка — по всем организациям.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn purge_removes_expired_dav_tombstones_and_vacation_log() {
        use tp_contracts::dav::{Collection, CollectionKind, DavStore};
        use tp_contracts::meta::{AccountScope, AccountStore, AccountUpsert};
        use tp_contracts::sieve::VacationLog;
        use tp_contracts::{DomainId, TenantId};

        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return;
        };
        let url = crate::testutil::dev_url(&url);
        let (db_name, url_db) = crate::testutil::create_database(&url).await;
        let pool = sqlx::PgPool::connect(&url_db).await.expect("pool");
        tp_meta::schema::migrate(&pool).await.expect("migrate");
        let ttl = Duration::from_secs(2);
        let mut cfg = tp_meta::MetaConfig::default();
        cfg.dav.tombstone_ttl = ttl;
        cfg.sieve.vacation_log_ttl = ttl;
        let meta = PgMeta::new(pool.clone(), cfg);

        let (tenant, domain) = (TenantId::new(), DomainId::new());
        let mut tx = tp_meta::schema::begin(&pool, tenant).await.expect("begin");
        sqlx::query("INSERT INTO tenants (tenant_id, name) VALUES ($1, 'test')")
            .bind(tenant.as_uuid())
            .execute(&mut *tx)
            .await
            .expect("tenant");
        sqlx::query(
            "INSERT INTO domains (tenant_id, domain_id, name) VALUES ($1, $2, 'purge.test')",
        )
        .bind(tenant.as_uuid())
        .bind(domain.as_uuid())
        .execute(&mut *tx)
        .await
        .expect("domain");
        tx.commit().await.expect("commit");
        let account = meta
            .upsert(
                tenant,
                AccountUpsert {
                    domain,
                    login: "user@purge.test".into(),
                    display_name: "User".into(),
                    external_id: None,
                    enabled: true,
                },
            )
            .await
            .expect("upsert");
        let scope = AccountScope {
            tenant,
            account: account.id,
        };
        // Надгробие — удалённая коллекция; запись журнала — отметка об автоответе отправителю.
        let tombstone = |name: &'static str| {
            let meta = meta.clone();
            async move {
                let book = Collection::new(CollectionKind::AddressBook, name);
                let id = book.id;
                meta.create_collection(scope, book).await.expect("create");
                meta.delete_collection(scope, id).await.expect("delete");
            }
        };
        let tombstones = || {
            let pool = pool.clone();
            async move {
                let mut tx = tp_meta::schema::begin(&pool, tenant).await.expect("begin");
                let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM dav_tombstones")
                    .fetch_one(&mut *tx)
                    .await
                    .expect("count");
                n
            }
        };
        let day = Duration::from_secs(24 * 3600);
        tombstone("old").await;
        assert!(
            !meta
                .seen(scope, "old@example.net", "", day)
                .await
                .expect("seen")
        );
        tokio::time::sleep(ttl + Duration::from_millis(500)).await;
        tombstone("fresh").await;
        assert!(
            !meta
                .seen(scope, "fresh@example.net", "", day)
                .await
                .expect("seen")
        );
        assert_eq!(tombstones().await, 2);

        let purge = Purge {
            meta: meta.clone(),
            interval: Duration::from_secs(3600),
        };
        let done = purge.step().await;
        assert_eq!(done.dav_tombstones.expect("dav"), 1);
        assert_eq!(done.vacation_log.expect("vacation"), 1);
        assert_eq!(tombstones().await, 1, "свежее надгробие на месте");
        assert!(
            meta.seen(scope, "fresh@example.net", "", day)
                .await
                .expect("seen"),
            "свежая запись журнала на месте"
        );
        assert!(
            !meta
                .seen(scope, "old@example.net", "", day)
                .await
                .expect("seen"),
            "старая запись удалена: отправителю снова отвечают"
        );

        // Роль: проход при запуске — после срока уходит и второе надгробие.
        tokio::time::sleep(ttl + Duration::from_millis(500)).await;
        let leases = MemLease::default();
        let role = Maintenance {
            index: Arc::new(MemBlobIndex::default()),
            store: Arc::new(MemBlobStore::default()),
            leases: Arc::new(leases.clone()),
            holder: "node-a/1".into(),
            events: EventConsumer::new(
                Arc::new(MemEventLog::new(1)) as Arc<dyn EventLog>,
                Arc::new(leases),
                "node-a/1",
                tp_meta::events::EventsConfig::default(),
            ),
            sinks: Vec::new(),
            signals: LogSignals {
                bus: Arc::new(tp_cluster::LocalBus::new(
                    &tp_cluster::LocalBusConfig::default(),
                )),
                members: None,
                lease_ttl: Duration::from_secs(30),
                resubscribe: Duration::from_secs(1),
            },
            gc: GcConfig::default(),
            orphans: OrphanConfig::default(),
            orphan_probe: OrphanProbe::default(),
            trash: None,
            purge: Some(purge),
        };
        let (ctx, shutdown, _ready) = ctx();
        let task = tokio::spawn(async move { role.run(ctx).await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tombstones().await != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "роль не выполнила очистку"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        shutdown.cancel();
        task.await.expect("join").expect("run");
        pool.close().await;
        crate::testutil::drop_database(&url, &db_name).await;
    }
}
