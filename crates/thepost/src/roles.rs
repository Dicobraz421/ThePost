//! Сервисы ролей ядра (SRS 2.1): maintenance — GC и «сироты» BLOB под лизой лидера, подписчики журнала событий;
//! роли протоколов, ещё не реализованные в этой сборке, — заглушки, готовые сразу (Ф0).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tp_blob::{GcConfig, GcWorker, OrphanConfig, OrphanSweeper, system_now};
use tp_contracts::Result;
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::events::EventSink;
use tp_contracts::lease::LeaseStore;
use tp_contracts::role::{CancellationToken, Role, RoleCtx, RoleService};
use tp_meta::events::EventConsumer;

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
    /// Период опроса журнала.
    pub events_poll: Duration,
    /// Сборка мусора объектов (`blob.gc`).
    pub gc: GcConfig,
    /// Поиск «сирот» (`blob.orphans`).
    pub orphans: OrphanConfig,
}

impl std::fmt::Debug for Maintenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Maintenance")
            .field("holder", &self.holder)
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
        let orphans = OrphanSweeper::new(
            self.index.clone(),
            self.store.clone(),
            self.orphans.clone(),
            system_now(),
        );
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
        ctx.ready.set(true);
        let (gc, orphans, ()) = tokio::join!(
            gc,
            orphans,
            consume(
                self.events.clone(),
                self.sinks.clone(),
                self.events_poll,
                ctx.shutdown.clone(),
            ),
        );
        for (what, done) in [("blob-gc", gc), ("blob-orphans", orphans)] {
            if let Err(e) = done {
                return Err(tp_contracts::Error::Internal(format!("{what}: {e}")));
            }
        }
        Ok(())
    }
}

/// Опрос журнала событий подписчиками до остановки; затем лизы разделов отдаются другим узлам.
async fn consume(
    events: EventConsumer,
    sinks: Vec<Arc<dyn EventSink>>,
    poll: Duration,
    shutdown: CancellationToken,
) {
    if sinks.is_empty() {
        return;
    }
    let mut tick = tokio::time::interval(poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            _ = tick.tick() => {}
        }
        for sink in &sinks {
            if let Err(e) = events.poll(sink.as_ref()).await {
                tracing::warn!(subscriber = sink.name(), error = %e, "журнал событий: пачка не обработана");
            }
        }
    }
    for sink in &sinks {
        if let Err(e) = events.release_all(sink.name()).await {
            tracing::warn!(subscriber = sink.name(), error = %e, "журнал событий: лизы разделов не отданы");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
            events_poll: Duration::from_millis(10),
            gc: GcConfig::default(),
            orphans: OrphanConfig::default(),
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
}
