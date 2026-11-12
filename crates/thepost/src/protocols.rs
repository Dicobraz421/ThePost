//! Роли протоколов и индексатора (APP-04): слушатели открывает приложение (`Listener::open`), сервисы — крейты
//! полос: smtp-in и submission (`tp-smtp`), imap (`tp-imap`: IMAP с QUOTA и ACL, POP3 по секции `imap.pop3`),
//! ManageSieve (`tp-sieve`) — в роли imap по секции `sieve.managesieve.listen`; jmap и admin (HTTP платформы), indexer (`tp-search` с обработчиком журнала событий `tp-meta`, чтение по сигналам —
//! [`crate::log_reader`]). Роль без
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
use tp_platform::{Config, InheritedSockets, Listener, TlsCerts, service_http};
use tp_search::{Indexer, IndexerConfig, Rebuilder, SCHEMA_VERSION, SUBSCRIBER};
use tp_smtp::sources::Sources;
use tp_smtp::{Mode, ServerDeps, SmtpConfig, SmtpService};

use crate::log_reader::{LogReader, LogSignals};
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

/// Открытие слушателей: сокет, переданный systemd для адреса (активация сокетов, `privileges.socket_activation`),
/// иначе — новый. Взятый сокет systemd остаётся у набора (копия дескриптора): перезапуск роли получает его снова,
/// а не делает bind (порты ниже 1024 без `CAP_NET_BIND_SERVICE` не открылись бы).
#[derive(Debug, Clone, Default)]
pub struct Sockets {
    inner: Arc<Mutex<Inherited>>,
    bound: Bound,
}

/// Сокеты systemd: ещё не взятые и взятые (по адресу слушателя из конфигурации).
#[derive(Debug, Default)]
struct Inherited {
    passed: InheritedSockets,
    taken: HashMap<SocketAddr, std::net::TcpListener>,
}

impl Sockets {
    /// Набор без сокетов systemd с реестром адресов `bound` (тесты).
    pub fn new(bound: Bound) -> Self {
        Self {
            inner: Arc::default(),
            bound,
        }
    }

    /// Набор с сокетами, переданными systemd процессу (`LISTEN_FDS`; дескрипторы берёт во владение
    /// `tp_sys::adopt_listen_fd`), по секции `privileges`. Вызывать один раз при старте, до открытия слушателей.
    /// Ошибка — запуск прервать: дескриптор юнита не слушающий TCP-сокет.
    pub fn adopt(cfg: &Config, bound: Bound) -> Result<Self> {
        let passed = tp_platform::privileges::inherited_sockets(cfg, tp_sys::adopt_listen_fd)?;
        Ok(Self::with_inherited(passed, bound))
    }

    /// Набор с готовыми сокетами `passed`.
    pub fn with_inherited(passed: InheritedSockets, bound: Bound) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inherited {
                passed,
                taken: HashMap::new(),
            })),
            bound,
        }
    }

    /// Реестр фактических адресов слушателей.
    pub fn bound(&self) -> &Bound {
        &self.bound
    }

    /// Сокеты systemd, которых не ждёт ни один адрес `wanted` (слушатели ролей узла), — в журнал (`warn`): лишние в
    /// юните сокета, соединения на них не принимаются. Возвращает их адреса.
    pub fn warn_unused(&self, wanted: &[SocketAddr]) -> Vec<SocketAddr> {
        let unused: Vec<SocketAddr> = self
            .lock()
            .passed
            .unused()
            .into_iter()
            .filter(|have| !wanted.iter().any(|want| serves(*want, *have)))
            .collect();
        for addr in &unused {
            tracing::warn!(
                %addr,
                "сокет от systemd не нужен ни одному слушателю ролей узла: соединения на нём не принимаются \
                 (уберите ListenStream из юнита сокета или задайте слушатель с этим адресом)"
            );
        }
        unused
    }

    /// Сокет systemd для адреса `listen` (копия дескриптора; `None` — сокета нет), неблокирующий — для tokio.
    pub fn take(&self, listen: SocketAddr) -> Result<Option<tokio::net::TcpListener>> {
        let mut inner = self.lock();
        let std = match inner.taken.get(&listen) {
            Some(kept) => kept.try_clone(),
            None => match inner.passed.take(listen) {
                Some(passed) => {
                    let copy = passed.try_clone();
                    inner.taken.insert(listen, passed);
                    copy
                }
                None => return Ok(None),
            },
        };
        drop(inner);
        std.and_then(|l| {
            l.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(l)
        })
        .map(Some)
        .map_err(|e| Error::Invalid(format!("listen {listen} (systemd): {e}")))
    }

    /// Сокет systemd для `listen`, иначе — `bind`.
    pub fn take_or(
        &self,
        listen: SocketAddr,
        bind: impl FnOnce() -> Result<tokio::net::TcpListener>,
    ) -> Result<tokio::net::TcpListener> {
        match self.take(listen)? {
            Some(tcp) => Ok(tcp),
            None => bind(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inherited> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
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
        let listeners = endpoints
            .iter()
            .map(|ep| match self.take(ep.listen)? {
                Some(tcp) => Listener::from_tcp(tcp, ep, tls.clone()),
                // Новый сокет; подсказка для портов ниже 1024 — у `Listener::open`.
                None => Listener::open(ep, tls.clone(), &mut InheritedSockets::default()),
            })
            .collect::<Result<Vec<_>>>()
            .map_err(|e| crate::infra::context(role, e))?;
        let addrs = listeners
            .iter()
            .filter_map(|l| l.local_addr().ok())
            .collect();
        self.bound.set(role, addrs);
        Ok(listeners)
    }
}

/// Слушатель с адресом `want` возьмёт сокет systemd `have` (правило `InheritedSockets::take`): тот же адрес или оба на
/// всех адресах с одним портом.
fn serves(want: SocketAddr, have: SocketAddr) -> bool {
    want == have
        || (want.ip().is_unspecified() && have.ip().is_unspecified() && want.port() == have.port())
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

/// Роль smtp-in или submission: секция `smtp` (`inbound`, `submission`), сервис `tp-smtp`; доверенные источники
/// приёма (шлюз, приложения) — из снимков настроек транспорта узла ([`SmtpRole::with_sources`]).
#[derive(Debug, Clone)]
pub struct SmtpRole {
    mode: Mode,
    deps: ServerDeps,
    sockets: Sockets,
    sources: Option<Sources>,
}

impl SmtpRole {
    /// Роль `mode` с зависимостями `deps`.
    pub fn new(mode: Mode, deps: ServerDeps, sockets: Sockets) -> Self {
        Self {
            mode,
            deps,
            sockets,
            sources: None,
        }
    }

    /// Доверенные источники приёма (SMTP-20): без них соединения — как от обычных клиентов.
    pub fn with_sources(mut self, sources: Sources) -> Self {
        self.sources = Some(sources);
        self
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
        let mut service = SmtpService::new(self.mode, self.deps.clone());
        if let Some(sources) = &self.sources {
            service = service.with_sources(sources.clone());
        }
        service.serve(listeners, ctx).await
    }
}

/// Имя слушателей POP3 в реестре адресов ([`Bound`]): POP3 работает в роли imap (секция `imap.pop3`).
pub const POP3: &str = "pop3";

/// Имя слушателей ManageSieve в реестре адресов ([`Bound`]): ManageSieve работает в роли imap (секция
/// `sieve.managesieve`, CONTRACTS.md#Sieve).
pub const MANAGESIEVE: &str = "managesieve";

/// ManageSieve в роли imap (APP-27): контракты сеансов, секция `sieve` и профиль узла (при `dev` вход без TLS).
/// Секция читается при запуске узла: слушатели (`sieve.managesieve.listen`) и лимиты сеанса требуют перезапуска.
#[derive(Debug, Clone)]
pub struct ManageSieveSetup {
    /// SASL (служба `sieve`, с защитой от перебора) и скрипты учётных записей.
    pub deps: tp_sieve::managesieve::Deps,
    /// Секция `sieve`.
    pub config: Arc<tp_sieve::SieveConfig>,
    /// Профиль узла.
    pub profile: Profile,
}

/// Роль imap: секция `imap` (`listeners`), сервер `tp-imap` с расширениями QUOTA, ACL и общими папками; POP3 —
/// по секции `imap.pop3` (`enabled`, `listeners`; STLS — сертификаты секции `tls`); ManageSieve — по секции
/// `sieve.managesieve.listen` ([`ImapRole::with_managesieve`]; пустой список — выключен).
#[derive(Debug, Clone)]
pub struct ImapRole {
    deps: tp_imap::Deps,
    ext: tp_imap::Extensions,
    tls: Option<Arc<TlsCerts>>,
    sockets: Sockets,
    sieve: Option<ManageSieveSetup>,
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
            sieve: None,
        }
    }

    /// ManageSieve (RFC 5804) на слушателях `sieve.managesieve.listen`; без вызова — не обслуживается.
    pub fn with_managesieve(mut self, sieve: ManageSieveSetup) -> Self {
        self.sieve = Some(sieve);
        self
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
        let sieve_endpoints: &[EndpointConfig] = match &self.sieve {
            Some(sieve) => &sieve.config.managesieve.listen,
            None => &[],
        };
        if cfg.listeners.is_empty() && pop3_endpoints.is_empty() && sieve_endpoints.is_empty() {
            return idle(self.name(), &ctx).await;
        }
        // Все слушатели открываются до приёма соединений: ошибка любого — ошибка запуска роли.
        let imap = self
            .sockets
            .open(self.name(), &cfg.listeners, self.tls.clone())?;
        let pop3 = self.sockets.open(POP3, pop3_endpoints, self.tls.clone())?;
        let sieve = self
            .sockets
            .open(MANAGESIEVE, sieve_endpoints, self.tls.clone())?;
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
        let server = tp_imap::Pop3Server::new(session()).with_accounts(self.ext.accounts.clone());
        for listener in pop3 {
            let server = server.clone();
            loops.spawn(async move { server.serve(listener).await });
        }
        if let Some(setup) = &self.sieve {
            let session = Arc::new(tp_sieve::managesieve::SessionCtx {
                config: setup.config.clone(),
                deps: setup.deps.clone(),
                profile: setup.profile,
                shutdown: ctx.shutdown.clone(),
            });
            for listener in sieve {
                let session = session.clone();
                let drain = ctx.drain_deadline;
                loops.spawn(async move {
                    tp_sieve::managesieve::serve(session, listener, drain).await;
                    Ok(())
                });
            }
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
    sockets: Sockets,
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
    /// Роль `role` с маршрутами `router` на слушателе `listener` (`None` — соединений не принимает); сокет — из
    /// `sockets` (systemd или новый).
    pub fn new(
        role: Role,
        router: axum::Router,
        listener: Option<HttpListener>,
        profile: Profile,
        tls: Option<Arc<TlsCerts>>,
        sockets: Sockets,
    ) -> Self {
        Self {
            role,
            router,
            listener,
            profile,
            tls,
            sockets,
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
        let tcp = self
            .sockets
            .take_or(server.listen, || service_http::bind(&server))
            .map_err(|e| crate::infra::context(&format!("http.{}", self.name()), e))?;
        if let Ok(addr) = tcp.local_addr() {
            self.sockets.bound.set(self.name(), vec![addr]);
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
    signals: LogSignals,
    rebuild_batch: usize,
    log_pause: LogPause,
}

impl std::fmt::Debug for IndexerRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexerRole")
            .field("signals", &self.signals)
            .finish_non_exhaustive()
    }
}

impl IndexerRole {
    /// Роль: журнал `events`, индексатор и перестройка; журнал дочитывается по сигналам `signals` (раздел с
    /// ошибкой — к сроку повтора), запросы перестройки — после каждого чтения; повторяющиеся ошибки — в журнал
    /// не чаще раза за паузу `log_pause`, перестройка с ошибкой повторяется через `log_pause.initial`.
    pub fn new(
        events: EventConsumer,
        indexer: Arc<Indexer>,
        rebuilder: Rebuilder,
        meta: PgMeta,
        signals: LogSignals,
        rebuild_batch: usize,
        log_pause: LogPause,
    ) -> Self {
        Self {
            events,
            indexer,
            rebuilder: Arc::new(rebuilder),
            meta,
            signals,
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
        let mut reader = LogReader::new(self.events.clone(), self.signals.clone());
        let sink = self.indexer.as_ref() as &dyn EventSink;
        while let Some(work) = reader.wait(&ctx.shutdown).await {
            let read = reader.read(sink, &work).await;
            log_step(&mut poll_log, read, "чтение журнала событий");
            let pending = self.rebuild_pending(&mut rebuild_failed).await;
            if pending.is_err() || !rebuild_failed.is_empty() {
                // Повтор при временной ошибке — один таймер (SRS 2.8, закрытый список, п. 2).
                reader.wake_in(self.log_pause.initial);
            }
            log_step(&mut pending_log, pending, "чтение запросов перестройки");
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

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncBufReadExt, BufReader};
    use tp_contracts::ext::DecisionHooks;
    use tp_contracts::load::{LoadLevel, fixed_load};
    use tp_contracts::role::{CancellationToken, ReadyHandle, fixed_config};
    use tp_testkit::{
        Clock, FakeAuthenticator, FakeSasl, FakeSearch, MemAcl, MemBlobIndex, MemBlobStore,
        MemEventBus, MemMailboxAudit, MemMetadata, MemQuota, MemTrash, SystemClock,
    };

    /// Хранилища testkit роли imap, общие с тестом: учётные записи, шина, вход.
    struct Stand {
        clock: Arc<dyn Clock>,
        index: MemBlobIndex,
        meta: MemMetadata,
        bus: MemEventBus,
        auth: FakeAuthenticator,
    }

    impl Stand {
        fn new() -> Self {
            let clock: Arc<dyn Clock> = Arc::new(SystemClock);
            let index = MemBlobIndex::new(clock.clone());
            let meta = MemMetadata::new(clock.clone()).with_blob_index(index.clone());
            Self {
                clock,
                index,
                meta,
                bus: MemEventBus::default(),
                auth: FakeAuthenticator::default(),
            }
        }
    }

    /// Роль imap на хранилищах `stand` с расширениями, как собирает их узел (`Services::imap_extensions`).
    fn imap_role(stand: &Stand, bound: Bound) -> ImapRole {
        let Stand {
            clock,
            index,
            meta,
            bus,
            auth,
        } = stand;
        let (meta, bus) = (meta.clone(), bus.clone());
        let trash = MemTrash::new(&meta);
        let store = tp_mailstore::Store::new(tp_mailstore::Deps {
            blobs: Arc::new(MemBlobStore::new(clock.clone())),
            index: Arc::new(index.clone()),
            messages: Arc::new(meta.clone()),
            changes: Arc::new(meta.clone()),
            trash: Arc::new(trash.clone()),
            quota: Arc::new(MemQuota::new(&meta)),
            bus: Arc::new(bus.clone()),
            hooks: DecisionHooks::default(),
        });
        let auth = Arc::new(auth.clone());
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
            audit: Arc::new(MemMailboxAudit::new()),
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
        let role = imap_role(&Stand::new(), bound.clone());
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

    /// APP-64: сигнал `AccountDisabled` потерян (обрыв потока шины), вход запрещён в хранилище — после
    /// переподписки POP3 сверяет сеансы с `Extensions::accounts` и закрывает сеанс учётной записи.
    #[tokio::test]
    async fn pop3_session_closed_after_reconcile_with_account_store() {
        use base64::Engine as _;
        use tokio::io::AsyncWriteExt;
        use tp_contracts::auth::Principal;
        use tp_contracts::meta::{AccountKind, AccountStore, AccountUpsert};
        use tp_contracts::{DomainId, TenantId};

        const LOGIN: &str = "pop3@example.org";
        let stand = Stand::new();
        let upsert = |enabled| AccountUpsert {
            domain: DomainId::new(),
            login: LOGIN.into(),
            display_name: "POP3".into(),
            external_id: None,
            enabled,
            aliases: Vec::new(),
            kind: AccountKind::User,
        };
        let tenant = TenantId::DEFAULT;
        let account = stand
            .meta
            .upsert(tenant, upsert(true))
            .await
            .expect("account");
        let principal = Principal {
            tenant,
            account: account.id,
            login: LOGIN.into(),
            groups: Default::default(),
            scopes: None,
        };
        stand.auth.add_token("pop3-token", principal);

        let mut cfg = tp_imap::ImapConfig {
            pop3: tp_imap::config::Pop3Config {
                enabled: true,
                listeners: vec![EndpointConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)))],
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.timeouts.resubscribe = Duration::from_millis(10);
        let bound = Bound::default();
        let role = imap_role(&stand, bound.clone());
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

        let pop3 = bound.get(POP3);
        assert_eq!(pop3.len(), 1, "{pop3:?}");
        let stream = tokio::net::TcpStream::connect(pop3[0])
            .await
            .expect("connect");
        let mut c = BufReader::new(stream);
        async fn line(c: &mut BufReader<tokio::net::TcpStream>) -> String {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(5), c.read_line(&mut line))
                .await
                .expect("no response in time")
                .expect("read");
            line
        }
        assert!(line(&mut c).await.starts_with("+OK"));
        // OAUTHBEARER не передаёт пароль и доступен без TLS.
        let creds = base64::engine::general_purpose::STANDARD
            .encode(format!("n,a={LOGIN},\x01auth=Bearer pop3-token\x01\x01"));
        c.get_mut()
            .write_all(format!("AUTH OAUTHBEARER {creds}\r\n").as_bytes())
            .await
            .expect("write");
        assert_eq!(line(&mut c).await, "+OK Logged in, 0 messages\r\n");

        // Вход запрещён, сигнал не опубликован: поток шины оборвался — после переподписки сверка с хранилищем.
        stand
            .meta
            .upsert(tenant, upsert(false))
            .await
            .expect("disable");
        stand.bus.disconnect();
        assert_eq!(line(&mut c).await, "-ERR [SYS/PERM] Account disabled\r\n");
        assert_eq!(line(&mut c).await, "", "соединение закрыто");

        shutdown.cancel();
        task.await.expect("join").expect("run");
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

    /// Сокеты systemd: слушатель берёт сокет своего адреса (перезапуск роли — снова его же, не bind), лишние —
    /// в предупреждение.
    #[tokio::test]
    async fn sockets_take_inherited_and_report_unused() {
        let passed = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let extra = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let (addr, extra_addr) = (
            passed.local_addr().expect("addr"),
            extra.local_addr().expect("addr"),
        );
        let mut fds = vec![extra, passed];
        let inherited = InheritedSockets::adopt(
            vec![
                tp_platform::privileges::InheritedFd { fd: 3, name: None },
                tp_platform::privileges::InheritedFd { fd: 4, name: None },
            ],
            |_| fds.pop().ok_or_else(|| std::io::Error::other("fd")),
        )
        .expect("adopt");
        let sockets = Sockets::with_inherited(inherited, Bound::default());
        assert_eq!(sockets.warn_unused(&[addr]), vec![extra_addr]);

        let endpoint = [EndpointConfig::new(addr)];
        for _restart in 0..2 {
            let listeners = sockets.open("smtp-in", &endpoint, None).expect("open");
            assert_eq!(sockets.bound().get("smtp-in"), vec![addr]);
            drop(listeners);
        }
        let any = SocketAddr::from(([0, 0, 0, 0], 0));
        assert!(sockets.take(any).expect("take").is_none());
        assert!(serves(
            SocketAddr::from(([0, 0, 0, 0], 25)),
            "[::]:25".parse().expect("addr")
        ));
        assert!(!serves(
            SocketAddr::from(([0, 0, 0, 0], 25)),
            SocketAddr::from(([127, 0, 0, 1], 25))
        ));
    }
}
