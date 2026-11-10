//! Роли протоколов и индексатора (APP-04): слушатели открывает приложение (`Listener::open`), сервисы — крейты
//! полос: smtp-in и submission (`tp-smtp`), imap (`tp-imap`: IMAP с QUOTA и ACL, POP3 по секции `imap.pop3`),
//! jmap и admin (HTTP платформы), indexer (`tp-search` с обработчиком журнала событий `tp-meta`). Роль без
//! слушателей в конфигурации готова, но соединений не принимает.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::task::JoinSet;
use tp_contracts::events::{EventLog, EventSink, partition_of};
use tp_contracts::meta::AccountScope;
use tp_contracts::role::{Role, RoleCtx, RoleService};
use tp_contracts::search::IndexManifestStore as _;
use tp_contracts::tls::Profile;
use tp_contracts::{Error, Result};
use tp_meta::PgMeta;
use tp_meta::events::EventConsumer;
use tp_platform::listen::EndpointConfig;
use tp_platform::{InheritedSockets, Listener, TlsCerts, service_http};
use tp_search::{Indexer, IndexerConfig, Rebuilder, SCHEMA_VERSION, SUBSCRIBER};
use tp_smtp::{Mode, ServerDeps, SmtpConfig, SmtpService};

use crate::sections::HttpListener;

/// Фактические адреса слушателей ролей узла (журнал, тесты: порт `0` — выбирает система).
#[derive(Debug, Clone, Default)]
pub struct Bound(Arc<Mutex<BTreeMap<String, Vec<SocketAddr>>>>);

impl Bound {
    pub(crate) fn set(&self, role: &str, addrs: Vec<SocketAddr>) {
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

    /// Слушатели роли `role` по `endpoints`; фактические адреса — в реестр (нет слушателей — реестр не меняется).
    fn open(
        &self,
        role: &str,
        endpoints: &[EndpointConfig],
        tls: Option<Arc<TlsCerts>>,
    ) -> Result<Vec<Listener>> {
        if endpoints.is_empty() {
            return Ok(Vec::new());
        }
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

/// Имя слушателей POP3 в реестре адресов ([`Bound`]): POP3 работает в роли imap (секция `imap.pop3`).
pub const POP3: &str = "pop3";

/// Роль imap: секция `imap` (`listeners`), сервер `tp-imap` с расширениями QUOTA, ACL и общими папками; POP3 —
/// по секции `imap.pop3` (`enabled`, `listeners`; STLS — сертификаты секции `tls`).
#[derive(Debug, Clone)]
pub struct ImapRole {
    deps: tp_imap::Deps,
    ext: tp_imap::Extensions,
    tls: Option<Arc<TlsCerts>>,
    sockets: Sockets,
}

impl ImapRole {
    /// Роль с зависимостями сессий `deps` и расширений `ext`.
    pub fn new(
        deps: tp_imap::Deps,
        ext: tp_imap::Extensions,
        tls: Option<Arc<TlsCerts>>,
        sockets: Sockets,
    ) -> Self {
        Self {
            deps,
            ext,
            tls,
            sockets,
        }
    }
}

#[async_trait]
impl RoleService for ImapRole {
    fn name(&self) -> &str {
        Role::Imap.as_str()
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        let cfg = ctx.config::<tp_imap::ImapConfig>()?;
        let pop3 = &cfg.pop3;
        match (pop3.enabled, pop3.listeners.is_empty()) {
            (true, true) => tracing::warn!("imap.pop3: POP3 включён, но слушатели не заданы"),
            (false, false) => tracing::info!("imap.pop3: слушатели заданы, но POP3 выключен"),
            _ => {}
        }
        let pop3_endpoints: &[EndpointConfig] = if pop3.enabled { &pop3.listeners } else { &[] };
        if cfg.listeners.is_empty() && pop3_endpoints.is_empty() {
            return idle(self.name(), &ctx).await;
        }
        // Все слушатели открываются до приёма соединений: ошибка любого — ошибка запуска роли.
        let imap = self
            .sockets
            .open(self.name(), &cfg.listeners, self.tls.clone())?;
        let pop3 = self.sockets.open(POP3, pop3_endpoints, self.tls.clone())?;
        let session = || tp_imap::SessionCtx {
            config: cfg.clone(),
            deps: self.deps.clone(),
            shutdown: ctx.shutdown.clone(),
        };
        let mut loops = JoinSet::new();
        let server = tp_imap::Server::new(session()).with_extensions(self.ext.clone());
        for listener in imap {
            let server = server.clone();
            loops.spawn(async move { server.serve(listener).await });
        }
        let server = tp_imap::Pop3Server::new(session());
        for listener in pop3 {
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
        let server = listener.server(self.role != Role::Admin);
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

/// Пауза между записями в журнал одной и той же ошибки: первая — `initial`, каждая следующая вдвое больше,
/// до `max` (как пауза повтора индексатора, `search.indexer.retry_initial`/`retry_max`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogPause {
    /// Пауза после первой записи.
    pub initial: Duration,
    /// Предельная пауза (меньше `initial` — `initial`).
    pub max: Duration,
}

impl From<&IndexerConfig> for LogPause {
    fn from(cfg: &IndexerConfig) -> Self {
        Self {
            initial: cfg.retry_initial,
            max: cfg.retry_max,
        }
    }
}

/// Повторяющаяся ошибка цикла роли: новая записывается в журнал сразу, та же — не чаще раза за паузу; пауза
/// растёт от записи к записи, успех сбрасывает состояние.
#[derive(Debug)]
struct RepeatLog {
    pause: LogPause,
    /// Последняя записанная ошибка, когда и пауза до следующей записи той же ошибки.
    last: Option<(String, Instant, Duration)>,
    /// Ошибок подряд с первой записи.
    failures: u64,
    /// Повторов, не записанных с прошлой записи.
    skipped: u64,
}

impl RepeatLog {
    fn new(pause: LogPause) -> Self {
        Self {
            pause,
            last: None,
            failures: 0,
            skipped: 0,
        }
    }

    /// Ошибка `msg` в `now`: `Some(пропущено повторов)` — записать в журнал.
    fn fail(&mut self, msg: String, now: Instant) -> Option<u64> {
        self.failures = self.failures.saturating_add(1);
        let max = self.pause.max.max(self.pause.initial);
        let pause = match &self.last {
            Some((last, at, pause)) if *last == msg => {
                if now.duration_since(*at) < *pause {
                    self.skipped = self.skipped.saturating_add(1);
                    return None;
                }
                pause.saturating_mul(2).min(max)
            }
            _ => self.pause.initial.min(max),
        };
        self.last = Some((msg, now, pause));
        Some(std::mem::take(&mut self.skipped))
    }

    /// Успешная попытка: ошибок подряд до неё (0 — ошибок не было); состояние сброшено.
    fn ok(&mut self) -> u64 {
        self.last = None;
        self.skipped = 0;
        std::mem::take(&mut self.failures)
    }
}

/// Итог шага цикла роли с записью в журнал по [`RepeatLog`]: `what` — что за шаг.
fn log_step(log: &mut RepeatLog, result: Result<()>, what: &str) {
    match result {
        Ok(()) => {
            let failures = log.ok();
            if failures > 0 {
                tracing::info!(failures, "indexer: {what} — снова без ошибок");
            }
        }
        Err(e) => {
            if let Some(skipped) = log.fail(e.to_string(), Instant::now()) {
                tracing::warn!(error = %e, skipped, "indexer: {what} — ошибка");
            }
        }
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
    log_pause: LogPause,
}

impl std::fmt::Debug for IndexerRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexerRole")
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

impl IndexerRole {
    /// Роль: журнал `events`, индексатор и перестройка; опрос журнала — каждые `poll`, а раздел с ошибкой —
    /// к сроку его повтора (`PollReport::retry_in`), если он раньше; повторяющиеся ошибки — в журнал не чаще
    /// раза за паузу `log_pause`.
    pub fn new(
        events: EventConsumer,
        indexer: Arc<Indexer>,
        rebuilder: Rebuilder,
        meta: PgMeta,
        poll: Duration,
        rebuild_batch: usize,
        log_pause: LogPause,
    ) -> Self {
        Self {
            events,
            indexer,
            rebuilder: Arc::new(rebuilder),
            meta,
            poll,
            rebuild_batch,
            log_pause,
        }
    }

    /// Перестройка запрошенных индексов ящиков, чьи разделы журнала держит узел; `failed` — ошибки перестройки
    /// по ящикам (остаются только ящики из текущего списка запросов).
    async fn rebuild_pending(&self, failed: &mut HashMap<AccountScope, RepeatLog>) -> Result<()> {
        let pending = self
            .meta
            .pending_rebuilds(SCHEMA_VERSION, self.rebuild_batch.max(1))
            .await?;
        failed.retain(|scope, _| pending.contains(scope));
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
                    failed.remove(&scope);
                    tracing::info!(account = %scope.account, messages = n, "search: индекс перестроен")
                }
                Err(e) => {
                    let log = failed
                        .entry(scope)
                        .or_insert_with(|| RepeatLog::new(self.log_pause));
                    if let Some(skipped) = log.fail(e.to_string(), Instant::now()) {
                        tracing::warn!(account = %scope.account, error = %e, skipped, "search: индекс не перестроен")
                    }
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
        let mut poll_log = RepeatLog::new(self.log_pause);
        let mut pending_log = RepeatLog::new(self.log_pause);
        let mut rebuild_failed = HashMap::new();
        let mut next = tokio::time::Instant::now();
        loop {
            tokio::select! {
                () = ctx.shutdown.cancelled() => break,
                () = tokio::time::sleep_until(next) => {}
            }
            let started = tokio::time::Instant::now();
            let polled = self
                .events
                .poll(self.indexer.as_ref() as &dyn EventSink)
                .await;
            let retry_at = polled
                .as_ref()
                .ok()
                .and_then(|r| r.retry_in)
                .map(|r| tokio::time::Instant::now() + r);
            log_step(&mut poll_log, polled.map(drop), "опрос журнала событий");
            let pending = self.rebuild_pending(&mut rebuild_failed).await;
            log_step(&mut pending_log, pending, "чтение запросов перестройки");
            next = next_poll(started, self.poll, retry_at);
        }
        if let Err(e) = self.events.release_all(SUBSCRIBER).await {
            tracing::warn!(error = %e, "indexer: лизы разделов не отданы");
        }
        ctx.ready.set(false);
        Ok(())
    }
}

/// Срок следующего опроса журнала: тик `poll` от начала прохода `started`, а если ближайший срок повтора раздела
/// с ошибкой `retry_at` (META-35) раньше — он; не раньше, чем через 1 мс после начала прохода.
fn next_poll(
    started: tokio::time::Instant,
    poll: Duration,
    retry_at: Option<tokio::time::Instant>,
) -> tokio::time::Instant {
    let tick = started + poll;
    retry_at
        .map_or(tick, |r| r.min(tick))
        .max(started + Duration::from_millis(1))
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

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncBufReadExt, BufReader};
    use tp_contracts::ext::DecisionHooks;
    use tp_contracts::load::{LoadLevel, fixed_load};
    use tp_contracts::role::{CancellationToken, ReadyHandle, fixed_config};
    use tp_testkit::{
        Clock, FakeAuthenticator, FakeSasl, FakeSearch, MemAcl, MemBlobIndex, MemBlobStore,
        MemEventBus, MemMetadata, MemQuota, MemTrash, SystemClock,
    };

    /// Роль imap на хранилищах testkit с расширениями, как собирает их узел (`Services::imap_extensions`).
    fn imap_role(bound: Bound) -> ImapRole {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let index = MemBlobIndex::new(clock.clone());
        let meta = MemMetadata::new(clock.clone()).with_blob_index(index.clone());
        let bus = MemEventBus::default();
        let trash = MemTrash::new(&meta);
        let store = tp_mailstore::Store::new(tp_mailstore::Deps {
            blobs: Arc::new(MemBlobStore::new(clock)),
            index: Arc::new(index),
            messages: Arc::new(meta.clone()),
            changes: Arc::new(meta.clone()),
            trash: Arc::new(trash.clone()),
            quota: Arc::new(MemQuota::new(&meta)),
            bus: Arc::new(bus.clone()),
            hooks: DecisionHooks::default(),
        });
        let auth = Arc::new(FakeAuthenticator::default());
        let deps = tp_imap::Deps {
            sasl: Arc::new(FakeSasl::new(auth.clone())),
            auth,
            mailboxes: Arc::new(meta.clone()),
            messages: Arc::new(meta.clone()),
            store: Arc::new(store),
            search: Arc::new(FakeSearch::default()),
            changes: Arc::new(meta.clone()),
            bus: Arc::new(bus),
            trash: Arc::new(trash),
            hooks: DecisionHooks::default(),
        };
        let acl = Arc::new(MemAcl::new(&meta));
        let accounts = Arc::new(meta);
        let ext = tp_imap::Extensions {
            accounts: Some(accounts.clone()),
            acl: Some(acl.clone()),
            authz: Some(Arc::new(tp_auth::AclAuthorizer::new(acl, accounts))),
        };
        ImapRole::new(deps, ext, None, Sockets::new(bound))
    }

    async fn first_line(addr: SocketAddr) -> String {
        let s = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).await.expect("read");
        line
    }

    /// APP-13: IMAP объявляет QUOTA и ACL, POP3 по секции `imap.pop3` отвечает `+OK`.
    #[tokio::test]
    async fn imap_role_announces_extensions_and_serves_pop3() {
        let local = EndpointConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)));
        let cfg = tp_imap::ImapConfig {
            listeners: vec![local.clone()],
            pop3: tp_imap::config::Pop3Config {
                enabled: true,
                listeners: vec![local],
                ..Default::default()
            },
            ..Default::default()
        };
        let bound = Bound::default();
        let role = imap_role(bound.clone());
        let shutdown = CancellationToken::new();
        let ready = ReadyHandle::default();
        let ctx = RoleCtx {
            config: fixed_config(cfg),
            shutdown: shutdown.clone(),
            drain_deadline: Duration::from_secs(1),
            ready: ready.clone(),
            load: fixed_load(LoadLevel::Normal),
        };
        let task = tokio::spawn(async move { role.run(ctx).await });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.is_ready() {
            assert!(Instant::now() < deadline, "роль imap не готова");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let imap = bound.get(Role::Imap.as_str());
        assert_eq!(imap.len(), 1, "{imap:?}");
        let greeting = first_line(imap[0]).await;
        assert!(greeting.starts_with("* OK [CAPABILITY "), "{greeting}");
        for cap in ["QUOTA", "ACL", "RIGHTS=texk"] {
            assert!(
                greeting.split([' ', ']']).any(|c| c == cap),
                "{cap}: {greeting}"
            );
        }

        let pop3 = bound.get(POP3);
        assert_eq!(pop3.len(), 1, "{pop3:?}");
        let greeting = first_line(pop3[0]).await;
        assert!(greeting.starts_with("+OK"), "{greeting}");

        shutdown.cancel();
        task.await.expect("join").expect("run");
        assert!(!ready.is_ready());
    }

    #[test]
    fn next_poll_waits_for_nearest_retry() {
        let t0 = tokio::time::Instant::now();
        let s = Duration::from_secs(1);
        // Без разделов с ошибкой и при сроке повтора позже тика — тик.
        assert_eq!(next_poll(t0, 5 * s, None), t0 + 5 * s);
        assert_eq!(next_poll(t0, 5 * s, Some(t0 + 9 * s)), t0 + 5 * s);
        // Срок повтора раньше тика — он.
        assert_eq!(next_poll(t0, 5 * s, Some(t0 + 2 * s)), t0 + 2 * s);
        // Срок уже наступил или тик нулевой — не раньше 1 мс от начала прохода (без холостого цикла).
        let floor = t0 + Duration::from_millis(1);
        assert_eq!(next_poll(t0, 5 * s, Some(t0)), floor);
        assert_eq!(next_poll(t0, Duration::ZERO, None), floor);
    }

    #[test]
    fn repeated_error_logged_once_per_pause() {
        let s = Duration::from_secs(1);
        let mut log = RepeatLog::new(LogPause {
            initial: s,
            max: 4 * s,
        });
        let t0 = Instant::now();
        let e = || "СУБД недоступна".to_owned();
        // Первая — сразу; повтор внутри паузы — пропущен; после паузы — запись, пауза удвоена.
        assert_eq!(log.fail(e(), t0), Some(0));
        assert_eq!(log.fail(e(), t0 + s / 2), None);
        assert_eq!(log.fail(e(), t0 + s), Some(1));
        assert_eq!(log.fail(e(), t0 + 2 * s), None);
        assert_eq!(log.fail(e(), t0 + 3 * s), Some(1));
        // Пауза не больше предела.
        assert_eq!(log.fail(e(), t0 + 7 * s), Some(0));
        assert_eq!(log.fail(e(), t0 + 11 * s), Some(0));
        // Другая ошибка — сразу, пауза — с начала.
        assert_eq!(log.fail("другая".to_owned(), t0 + 11 * s), Some(0));
        assert_eq!(log.fail("другая".to_owned(), t0 + 12 * s), Some(0));
        // Успех: ошибок подряд, состояние сброшено.
        assert_eq!(log.ok(), 9);
        assert_eq!(log.ok(), 0);
        assert_eq!(log.fail(e(), t0 + 12 * s), Some(0));
    }
}
