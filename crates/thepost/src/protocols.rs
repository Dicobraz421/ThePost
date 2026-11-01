//! Роли протоколов и индексатора (APP-04): слушатели открывает приложение (`Listener::open`), сервисы — крейты
//! полос: smtp-in и submission (`tp-smtp`), imap (`tp-imap`), jmap и admin (HTTP платформы), indexer (`tp-search`
//! с обработчиком журнала событий `tp-meta`). Роль без слушателей в конфигурации готова, но соединений не принимает.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::task::JoinSet;
use tp_contracts::events::{EventLog, EventSink, partition_of};
use tp_contracts::role::{Role, RoleCtx, RoleService};
use tp_contracts::search::IndexManifestStore as _;
use tp_contracts::tls::Profile;
use tp_contracts::{Error, Result};
use tp_meta::PgMeta;
use tp_meta::events::EventConsumer;
use tp_platform::listen::EndpointConfig;
use tp_platform::{InheritedSockets, Listener, TlsCerts, service_http};
use tp_search::{Indexer, Rebuilder, SCHEMA_VERSION, SUBSCRIBER};
use tp_smtp::{Mode, ServerDeps, SmtpConfig, SmtpService};

use crate::sections::HttpListener;

/// Фактические адреса слушателей ролей узла (журнал, тесты: порт `0` — выбирает система).
#[derive(Debug, Clone, Default)]
pub struct Bound(Arc<Mutex<BTreeMap<String, Vec<SocketAddr>>>>);

impl Bound {
    fn set(&self, role: &str, addrs: Vec<SocketAddr>) {
        tracing::info!(role, ?addrs, "роль принимает соединения");
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(role.to_owned(), addrs);
    }

    /// Адреса слушателей роли `role`; роль ещё не открыла их — пусто.
    pub fn get(&self, role: &str) -> Vec<SocketAddr> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(role)
            .cloned()
            .unwrap_or_default()
    }
}

/// Открытие слушателей: сокет, переданный systemd для адреса, иначе — новый.
#[derive(Debug, Clone, Default)]
pub struct Sockets {
    // TODO(PLT-16): приём дескрипторов systemd (`listen_fds` → `InheritedSockets::adopt`) требует функции
    // «номер дескриптора → TcpListener» с `unsafe` (`FromRawFd`): ей место в tp-sys (x/-PR) или зависимость
    // `listenfd`. Пока набор пуст — `Listener::open` делает bind.
    inherited: Arc<Mutex<InheritedSockets>>,
    bound: Bound,
}

impl Sockets {
    /// Набор с реестром адресов `bound`.
    pub fn new(bound: Bound) -> Self {
        Self {
            inherited: Arc::default(),
            bound,
        }
    }

    /// Слушатели роли `role` по `endpoints`; фактические адреса — в реестр.
    fn open(
        &self,
        role: &str,
        endpoints: &[EndpointConfig],
        tls: Option<Arc<TlsCerts>>,
    ) -> Result<Vec<Listener>> {
        let mut inherited = self
            .inherited
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let listeners = endpoints
            .iter()
            .map(|ep| Listener::open(ep, tls.clone(), &mut inherited))
            .collect::<Result<Vec<_>>>()
            .map_err(|e| crate::infra::context(role, e))?;
        drop(inherited);
        let addrs = listeners
            .iter()
            .filter_map(|l| l.local_addr().ok())
            .collect();
        self.bound.set(role, addrs);
        Ok(listeners)
    }
}

/// Роль без слушателей: готова и ждёт остановки (узел с пресетом доходит до ready).
async fn idle(name: &str, ctx: &RoleCtx) -> Result<()> {
    tracing::info!(
        role = name,
        "слушатели роли не заданы: соединений не принимает"
    );
    ctx.ready.set(true);
    ctx.shutdown.cancelled().await;
    ctx.ready.set(false);
    Ok(())
}

/// Роль smtp-in или submission: секция `smtp` (`inbound`, `submission`), сервис `tp-smtp`.
#[derive(Debug, Clone)]
pub struct SmtpRole {
    mode: Mode,
    deps: ServerDeps,
    sockets: Sockets,
}

impl SmtpRole {
    /// Роль `mode` с зависимостями `deps`.
    pub fn new(mode: Mode, deps: ServerDeps, sockets: Sockets) -> Self {
        Self {
            mode,
            deps,
            sockets,
        }
    }
}

#[async_trait]
impl RoleService for SmtpRole {
    fn name(&self) -> &str {
        self.mode.name()
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        let cfg = ctx.config::<SmtpConfig>()?;
        let endpoints = match self.mode {
            Mode::Inbound => &cfg.inbound,
            Mode::Submission => &cfg.submission,
        };
        if endpoints.is_empty() {
            return idle(self.name(), &ctx).await;
        }
        if self.mode == Mode::Submission && self.deps.tls.is_none() {
            return Err(Error::invalid(
                "smtp.submission: нужны сертификаты TLS (секция tls): AUTH — только по TLS",
            ));
        }
        let listeners = self
            .sockets
            .open(self.name(), endpoints, self.deps.tls.clone())?;
        SmtpService::new(self.mode, self.deps.clone())
            .serve(listeners, ctx)
            .await
    }
}

/// Роль imap: секция `imap` (`listeners`), сервер `tp-imap`.
#[derive(Debug, Clone)]
pub struct ImapRole {
    deps: tp_imap::Deps,
    tls: Option<Arc<TlsCerts>>,
    sockets: Sockets,
}

impl ImapRole {
    /// Роль с зависимостями сессий `deps`.
    pub fn new(deps: tp_imap::Deps, tls: Option<Arc<TlsCerts>>, sockets: Sockets) -> Self {
        Self { deps, tls, sockets }
    }
}

#[async_trait]
impl RoleService for ImapRole {
    fn name(&self) -> &str {
        Role::Imap.as_str()
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        let cfg = ctx.config::<tp_imap::ImapConfig>()?;
        if cfg.listeners.is_empty() {
            return idle(self.name(), &ctx).await;
        }
        let listeners = self
            .sockets
            .open(self.name(), &cfg.listeners, self.tls.clone())?;
        let server = tp_imap::Server::new(tp_imap::SessionCtx {
            config: cfg.clone(),
            deps: self.deps.clone(),
            shutdown: ctx.shutdown.clone(),
        });
        let mut loops = JoinSet::new();
        for listener in listeners {
            let server = server.clone();
            loops.spawn(async move { server.serve(listener).await });
        }
        ctx.ready.set(true);
        let mut result = Ok(());
        while let Some(done) = loops.join_next().await {
            let done = done
                .map_err(|e| Error::Internal(format!("imap: {e}")))
                .and_then(|r| r);
            if let Err(e) = done
                && result.is_ok()
            {
                result = Err(e);
            }
        }
        ctx.ready.set(false);
        result
    }
}

/// Действие при начале остановки узла (JMAP — закрыть соединения push, иначе они держат drain).
pub type OnStop = Arc<dyn Fn() + Send + Sync>;

/// Роль jmap или admin: маршруты крейта полосы на сервере HTTP платформы (TLS — секция `tls`; открытый HTTP —
/// только `profile: dev` с `allow_plaintext`), слушатель — секция `http`.
#[derive(Clone)]
pub struct HttpRole {
    role: Role,
    router: axum::Router,
    listener: Option<HttpListener>,
    profile: Profile,
    tls: Option<Arc<TlsCerts>>,
    bound: Bound,
    on_stop: Option<OnStop>,
}

impl std::fmt::Debug for HttpRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRole")
            .field("role", &self.role)
            .field("listener", &self.listener)
            .finish_non_exhaustive()
    }
}

impl HttpRole {
    /// Роль `role` с маршрутами `router` на слушателе `listener` (`None` — соединений не принимает).
    pub fn new(
        role: Role,
        router: axum::Router,
        listener: Option<HttpListener>,
        profile: Profile,
        tls: Option<Arc<TlsCerts>>,
        bound: Bound,
    ) -> Self {
        Self {
            role,
            router,
            listener,
            profile,
            tls,
            bound,
            on_stop: None,
        }
    }

    /// Действие при начале остановки.
    pub fn on_stop(mut self, f: OnStop) -> Self {
        self.on_stop = Some(f);
        self
    }
}

#[async_trait]
impl RoleService for HttpRole {
    fn name(&self) -> &str {
        self.role.as_str()
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        let Some(listener) = &self.listener else {
            return idle(self.name(), &ctx).await;
        };
        let server = listener.server();
        let tcp = service_http::bind(&server)
            .map_err(|e| crate::infra::context(&format!("http.{}", self.name()), e))?;
        if let Ok(addr) = tcp.local_addr() {
            self.bound.set(self.name(), vec![addr]);
        }
        if let Some(stop) = self.on_stop.clone() {
            let shutdown = ctx.shutdown.clone();
            tokio::spawn(async move {
                shutdown.cancelled().await;
                stop();
            });
        }
        ctx.ready.set(true);
        let done = service_http::serve(
            tcp,
            self.router.clone(),
            &server,
            self.profile,
            self.tls.clone(),
            ctx.shutdown.clone(),
        )
        .await
        .map_err(|e| crate::infra::context(&format!("http.{}", self.name()), e));
        ctx.ready.set(false);
        done
    }
}

/// Роль indexer: подписчик `indexer` журнала событий с лизами разделов `events/indexer/{p}` (обработчик
/// `tp-meta`), перестройка индексов по `pending_rebuilds` для ящиков своих разделов.
pub struct IndexerRole {
    events: EventConsumer,
    indexer: Arc<Indexer>,
    rebuilder: Arc<Rebuilder>,
    meta: PgMeta,
    poll: Duration,
    rebuild_batch: usize,
}

impl std::fmt::Debug for IndexerRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexerRole")
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

impl IndexerRole {
    /// Роль: журнал `events`, индексатор и перестройка; опрос журнала — каждые `poll`.
    pub fn new(
        events: EventConsumer,
        indexer: Indexer,
        rebuilder: Rebuilder,
        meta: PgMeta,
        poll: Duration,
        rebuild_batch: usize,
    ) -> Self {
        Self {
            events,
            indexer: Arc::new(indexer),
            rebuilder: Arc::new(rebuilder),
            meta,
            poll,
            rebuild_batch,
        }
    }

    /// Перестройка запрошенных индексов ящиков, чьи разделы журнала держит узел.
    async fn rebuild_pending(&self) -> Result<()> {
        let pending = self
            .meta
            .pending_rebuilds(SCHEMA_VERSION, self.rebuild_batch.max(1))
            .await?;
        if pending.is_empty() {
            return Ok(());
        }
        let held = self.events.held(SUBSCRIBER);
        let partitions = EventLog::partitions(&self.meta);
        for scope in pending {
            if !held.contains(&partition_of(scope.account, partitions)) {
                continue;
            }
            match self.rebuilder.rebuild(scope).await {
                Ok(n) => {
                    tracing::info!(account = %scope.account, messages = n, "search: индекс перестроен")
                }
                Err(e) => {
                    tracing::warn!(account = %scope.account, error = %e, "search: индекс не перестроен")
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl RoleService for IndexerRole {
    fn name(&self) -> &str {
        Role::Indexer.as_str()
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        ctx.ready.set(true);
        let mut tick = tokio::time::interval(self.poll.max(Duration::from_millis(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = ctx.shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }
            if let Err(e) = self
                .events
                .poll(self.indexer.as_ref() as &dyn EventSink)
                .await
            {
                tracing::warn!(error = %e, "indexer: пачка журнала событий не обработана");
            }
            if let Err(e) = self.rebuild_pending().await {
                tracing::warn!(error = %e, "indexer: запросы перестройки не прочитаны");
            }
        }
        if let Err(e) = self.events.release_all(SUBSCRIBER).await {
            tracing::warn!(error = %e, "indexer: лизы разделов не отданы");
        }
        ctx.ready.set(false);
        Ok(())
    }
}

/// Роль, работа которой в этой сборке идёт в процессе других ролей (search: запросы выполняют imap и jmap
/// узла через общий индекс). Готова и ждёт остановки.
#[derive(Debug, Clone, Copy)]
pub struct InProcessRole(pub Role);

#[async_trait]
impl RoleService for InProcessRole {
    fn name(&self) -> &str {
        self.0.as_str()
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        tracing::info!(role = %self.0, "роль выполняется в процессе ролей протоколов узла");
        ctx.ready.set(true);
        ctx.shutdown.cancelled().await;
        ctx.ready.set(false);
        Ok(())
    }
}
