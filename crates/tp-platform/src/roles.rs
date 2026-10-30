//! Рантайм ролей (SRS 2.1, 2.7, CONTRACTS.md#Roles): запуск набора [`RoleService`] по конфигурации,
//! лимиты кэшей и пулов по ролям от ресурсов узла, штатная остановка с drain, метрика RSS процесса.
//!
//! Ленивая инициализация: сервис роли создаётся фабрикой только для включённой роли, поэтому невключённая
//! роль не создаёт пулов, кэшей, потоков и портов. Остановка: сигнал → узел выводится из пула (`/healthz/ready`
//! 503) → роли получают `ctx.shutdown` и завершают работу за `drain_deadline` → оставшиеся задачи отменяются.
//!
//! Аварийное завершение роли (ошибка или паника) не останавливает остальные роли: роль снимается с готовности и
//! перезапускается с нарастающей паузой. Больше `restart.max_restarts` падений одной роли за `restart.window` —
//! узел останавливается штатно, дальше процесс перезапускает systemd или Kubernetes.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::task::{Id, JoinError, JoinSet};
use tokio::time::Instant;
use tp_contracts::Error;
use tp_contracts::config::duration;
use tp_contracts::role::{
    CancellationToken, ConfigRx, ReadyHandle, Role, RoleCtx, RoleService, RoleSet,
};
use tp_sys::NodeResources;

use crate::config::Config;
use crate::service_http::Health;
use crate::telemetry::Metrics;

/// Секция `runtime`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    /// Время на завершение текущей работы ролей после сигнала остановки (`30s`); затем задачи отменяются.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub drain_deadline: Duration,
    /// Перезапуск упавшей роли внутри процесса.
    pub restart: RestartConfig,
    /// Доля памяти узла под кэши всех включённых ролей, %. Делится поровну между ролями. Требует перезапуска.
    pub cache_memory_percent: u8,
    /// Соединений с PostgreSQL на один CPU узла. Делятся поровну между ролями. Требует перезапуска.
    pub db_pool_per_cpu: u32,
    /// Предел соединений с PostgreSQL на узел. Требует перезапуска.
    pub db_pool_max: u32,
    /// Явные лимиты отдельных ролей вместо рассчитанных. Требует перезапуска.
    pub limits: BTreeMap<Role, RoleLimitsOverride>,
    /// Период обновления метрики RSS процесса (`15s`); `0s` — метрика не обновляется.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub rss_interval: Duration,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            drain_deadline: Duration::from_secs(30),
            restart: RestartConfig::default(),
            cache_memory_percent: 25,
            db_pool_per_cpu: 4,
            db_pool_max: 64,
            limits: BTreeMap::new(),
            rss_interval: Duration::from_secs(15),
        }
    }
}

/// Подсекция `runtime.restart`: перезапуск упавшей роли с нарастающей паузой.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RestartConfig {
    /// Перезапусков одной роли за окно; следующее падение останавливает узел. 0 — первое падение останавливает узел.
    pub max_restarts: u32,
    /// Окно подсчёта падений (`10m`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub window: Duration,
    /// Пауза перед первым перезапуском (`1s`); каждое следующее падение в окне удваивает её.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub backoff_initial: Duration,
    /// Предел паузы перед перезапуском (`1m`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub backoff_max: Duration,
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            max_restarts: 5,
            window: Duration::from_secs(600),
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
        }
    }
}

impl RestartConfig {
    /// Пауза перед перезапуском после `failures`-го падения в окне (с 1).
    pub fn backoff(&self, failures: u32) -> Duration {
        let factor = 1u32
            .checked_shl(failures.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.backoff_initial
            .saturating_mul(factor)
            .min(self.backoff_max)
    }
}

/// Явные лимиты роли; незаданное поле рассчитывается.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RoleLimitsOverride {
    /// Память под кэши роли, байт.
    pub cache_bytes: Option<u64>,
    /// Соединений с PostgreSQL.
    pub db_pool: Option<u32>,
}

/// Лимиты ресурсов роли: размеры кэшей и пулов, которые роль создаёт внутри `run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleLimits {
    /// Память под кэши роли, байт.
    pub cache_bytes: u64,
    /// Соединений с PostgreSQL.
    pub db_pool: u32,
}

/// Лимиты включённых ролей от ресурсов узла (SRS 2.7). Невключённые роли в результат не входят.
pub fn role_limits(
    cfg: &RuntimeConfig,
    roles: &RoleSet,
    node: &NodeResources,
) -> BTreeMap<Role, RoleLimits> {
    let n = roles.roles.len().max(1) as u64;
    let cache_total = node.memory_total / 100 * u64::from(cfg.cache_memory_percent.min(100));
    let cpus = u32::try_from(node.cpus_ceil()).unwrap_or(u32::MAX);
    let pool_total = cpus
        .saturating_mul(cfg.db_pool_per_cpu)
        .min(cfg.db_pool_max);
    let pool_each = u32::try_from(u64::from(pool_total) / n)
        .unwrap_or(u32::MAX)
        .max(1);
    roles
        .roles
        .iter()
        .map(|role| {
            let o = cfg.limits.get(role).copied().unwrap_or_default();
            let limits = RoleLimits {
                cache_bytes: o.cache_bytes.unwrap_or(cache_total / n),
                db_pool: o.db_pool.unwrap_or(pool_each),
            };
            (*role, limits)
        })
        .collect()
}

/// Сервис включённой роли и источник её секции конфигурации.
pub struct RoleSpec {
    /// Сервис роли.
    pub service: Arc<dyn RoleService>,
    /// Секция конфигурации роли с hot reload.
    pub config: ConfigRx,
}

impl fmt::Debug for RoleSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoleSpec")
            .field("service", &self.service.name())
            .finish_non_exhaustive()
    }
}

/// Итог остановки ролей.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RolesReport {
    /// Роли, завершившиеся штатно.
    pub completed: Vec<String>,
    /// Роли, не уложившиеся в `drain_deadline` и отменённые.
    pub cancelled: Vec<String>,
    /// Роли, завершившиеся ошибкой или паникой без перезапуска: превышен лимит перезапусков или сбой при drain.
    pub failed: Vec<(String, Error)>,
    /// Число перезапусков по ролям.
    pub restarts: BTreeMap<String, u32>,
}

/// Набор ролей узла до запуска.
pub struct Roles {
    cfg: RuntimeConfig,
    set: RoleSet,
    limits: BTreeMap<Role, RoleLimits>,
    specs: Vec<RoleSpec>,
    updates: Option<watch::Receiver<Arc<Config>>>,
}

/// Запущенная роль: всё для перезапуска и история падений.
struct Slot {
    name: String,
    service: Arc<dyn RoleService>,
    config: ConfigRx,
    ready: ReadyHandle,
    failures: VecDeque<Instant>,
}

impl Slot {
    /// Запускает роль после паузы `delay`; сигнал остановки во время паузы завершает задачу штатно.
    fn spawn(
        &self,
        tasks: &mut JoinSet<tp_contracts::Result<()>>,
        shutdown: &CancellationToken,
        deadline: Duration,
        delay: Duration,
    ) -> Id {
        let service = self.service.clone();
        let ctx = RoleCtx {
            config: self.config.clone(),
            shutdown: shutdown.clone(),
            drain_deadline: deadline,
            ready: self.ready.clone(),
        };
        tasks
            .spawn(async move {
                if !delay.is_zero() {
                    tokio::select! {
                        () = ctx.shutdown.cancelled() => return Ok(()),
                        () = tokio::time::sleep(delay) => {}
                    }
                }
                service.run(ctx).await
            })
            .id()
    }

    /// Учитывает падение в момент `now`; возвращает число падений за окно `window`.
    fn fail(&mut self, now: Instant, window: Duration) -> u32 {
        while self
            .failures
            .front()
            .is_some_and(|t| now.duration_since(*t) >= window)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        u32::try_from(self.failures.len()).unwrap_or(u32::MAX)
    }
}

impl fmt::Debug for Roles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Roles")
            .field("roles", &self.set.roles)
            .field("specs", &self.specs)
            .finish_non_exhaustive()
    }
}

impl Roles {
    /// Набор для ролей `set` с лимитами от ресурсов `node`.
    pub fn new(cfg: RuntimeConfig, set: RoleSet, node: &NodeResources) -> Self {
        let limits = role_limits(&cfg, &set, node);
        Self {
            cfg,
            set,
            limits,
            specs: Vec::new(),
            updates: None,
        }
    }

    /// Новые `runtime.restart` и `runtime.drain_deadline` применяются на ходу из перечитанной конфигурации.
    pub fn with_updates(mut self, rx: watch::Receiver<Arc<Config>>) -> Self {
        self.updates = Some(rx);
        self
    }

    /// Лимиты включённой роли; `None` — роль не включена.
    pub fn limits(&self, role: Role) -> Option<RoleLimits> {
        self.limits.get(&role).copied()
    }

    /// Добавляет роль ядра: фабрика вызывается, только если роль включена. Возвращает признак включения.
    pub fn add(&mut self, role: Role, make: impl FnOnce(RoleLimits) -> RoleSpec) -> bool {
        match self.limits(role) {
            Some(limits) => {
                self.specs.push(make(limits));
                true
            }
            None => false,
        }
    }

    /// Добавляет роль модуля (включение решает вызывающий по `modules.<имя>.enabled`).
    pub fn add_module(&mut self, spec: RoleSpec) {
        self.specs.push(spec);
    }

    /// Имена добавленных ролей.
    pub fn names(&self) -> Vec<String> {
        self.specs
            .iter()
            .map(|s| s.service.name().to_owned())
            .collect()
    }

    /// Запускает роли и работает до сигнала `shutdown`, затем останавливает все роли.
    ///
    /// Упавшая роль снимается с готовности и перезапускается после паузы `restart.backoff`. Больше
    /// `restart.max_restarts` падений за окно — узел останавливается: остальные роли проходят drain, ошибка — в отчёте.
    pub async fn run(
        self,
        health: &Health,
        metrics: &Metrics,
        shutdown: CancellationToken,
    ) -> RolesReport {
        let mut cfg = self.cfg;
        let mut updates = self.updates;
        let running = Gauge::<i64>::default();
        metrics.register("tp_roles_running", "Running role tasks", running.clone());
        let restarts = Family::<Vec<(String, String)>, Counter>::default();
        metrics.register(
            "tp_role_restarts",
            "Role restarts after failure",
            restarts.clone(),
        );

        let role_shutdown = CancellationToken::new();
        let mut tasks = JoinSet::new();
        let mut slots: Vec<Slot> = Vec::new();
        let mut ids: BTreeMap<Id, usize> = BTreeMap::new();
        for spec in self.specs {
            let name = spec.service.name().to_owned();
            let slot = Slot {
                ready: health.register(&name),
                name,
                service: spec.service,
                config: spec.config,
                failures: VecDeque::new(),
            };
            let id = slot.spawn(
                &mut tasks,
                &role_shutdown,
                cfg.drain_deadline,
                Duration::ZERO,
            );
            ids.insert(id, slots.len());
            slots.push(slot);
        }
        running.set(tasks.len() as i64);
        tracing::info!(roles = ?slots.iter().map(|s| &s.name).collect::<Vec<_>>(), "roles started");

        let mut report = RolesReport::default();
        // До сигнала: роль, завершившаяся сама, — штатно (Ok) или аварийно (перезапуск либо остановка узла).
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                new = config_changed(&mut updates) => {
                    if let Some(new) = new {
                        cfg.restart = new.restart.clone();
                        cfg.drain_deadline = new.drain_deadline;
                    }
                }
                done = tasks.join_next_with_id() => {
                    let Some(done) = done else { break };
                    running.set(tasks.len() as i64);
                    let (id, result) = outcome(done);
                    let Some(&idx) = ids.get(&id) else { continue };
                    let slot = &mut slots[idx];
                    let err = match result {
                        Ok(()) => {
                            report.completed.push(slot.name.clone());
                            continue;
                        }
                        Err(err) => err,
                    };
                    slot.ready.set(false);
                    let failures = slot.fail(Instant::now(), cfg.restart.window);
                    if failures > cfg.restart.max_restarts {
                        tracing::error!(role = %slot.name, error = %err, failures, "role restart limit exceeded, stopping node");
                        report.failed.push((slot.name.clone(), err));
                        break;
                    }
                    let delay = cfg.restart.backoff(failures);
                    tracing::warn!(role = %slot.name, error = %err, failures, delay_ms = delay.as_millis() as u64, "role failed, restarting");
                    restarts
                        .get_or_create(&vec![("role".to_owned(), slot.name.clone())])
                        .inc();
                    *report.restarts.entry(slot.name.clone()).or_default() += 1;
                    let id = slot.spawn(&mut tasks, &role_shutdown, cfg.drain_deadline, delay);
                    ids.insert(id, idx);
                    running.set(tasks.len() as i64);
                }
            }
        }

        let deadline = cfg.drain_deadline;
        tracing::info!(deadline_ms = deadline.as_millis() as u64, "roles draining");
        health.drain();
        for slot in &slots {
            slot.ready.set(false);
        }
        role_shutdown.cancel();

        let name_of = |id: Id| {
            ids.get(&id)
                .map(|&i| slots[i].name.clone())
                .unwrap_or_default()
        };
        let finish = |report: &mut RolesReport, done| match outcome(done) {
            (id, Ok(())) => report.completed.push(name_of(id)),
            (id, Err(err)) => {
                let name = name_of(id);
                tracing::error!(role = %name, error = %err, "role failed during drain");
                report.failed.push((name, err));
            }
        };
        let drained = tokio::time::timeout(deadline, async {
            while let Some(done) = tasks.join_next_with_id().await {
                running.set(tasks.len() as i64);
                finish(&mut report, done);
            }
        })
        .await;
        if drained.is_err() {
            tasks.abort_all();
            while let Some(done) = tasks.join_next_with_id().await {
                match done {
                    Err(e) if e.is_cancelled() => {
                        let name = name_of(e.id());
                        tracing::warn!(role = %name, "role cancelled after drain deadline");
                        report.cancelled.push(name);
                    }
                    done => finish(&mut report, done),
                }
            }
        }
        running.set(0);
        report
    }
}

/// Результат задачи роли; паника и отмена — ошибки.
fn outcome(
    done: Result<(Id, tp_contracts::Result<()>), JoinError>,
) -> (Id, tp_contracts::Result<()>) {
    match done {
        Ok(done) => done,
        Err(e) if e.is_panic() => (e.id(), Err(Error::internal("role panicked"))),
        Err(e) => (e.id(), Err(Error::internal("role task cancelled"))),
    }
}

/// Секция `runtime` после перечитывания конфигурации; без источника обновлений ждёт вечно.
async fn config_changed(
    updates: &mut Option<watch::Receiver<Arc<Config>>>,
) -> Option<Arc<RuntimeConfig>> {
    let Some(rx) = updates else {
        return std::future::pending().await;
    };
    if rx.changed().await.is_err() {
        *updates = None;
        return None;
    }
    let section = rx.borrow_and_update().section::<RuntimeConfig>("runtime");
    section
        .inspect_err(|e| tracing::warn!(error = %e, "runtime section unavailable"))
        .ok()
}

/// Ждёт SIGTERM или SIGINT; SIGHUP пропускает (его обрабатывает hot reload). Требует контекста tokio-рантайма.
pub async fn shutdown_signal() -> tp_contracts::Result<()> {
    let mut signals =
        tp_sys::Signals::new().map_err(|e| Error::Internal(format!("signal handlers: {e}")))?;
    while let Some(sig) = signals.recv().await {
        if sig.is_shutdown() {
            tracing::info!(signal = ?sig, "shutdown requested");
            return Ok(());
        }
    }
    Err(Error::UNAVAILABLE)
}

/// Метрика резидентной памяти процесса `tp_process_resident_memory_bytes` (SRS 2.7: цель ≤ 64 MB на роль без нагрузки).
#[derive(Debug, Clone, Default)]
pub struct RssGauge(Gauge<i64>);

impl RssGauge {
    /// Регистрирует метрику в реестре.
    pub fn register(metrics: &Metrics) -> Self {
        let gauge = Self::default();
        metrics.register(
            "tp_process_resident_memory_bytes",
            "Resident set size of the process in bytes",
            gauge.0.clone(),
        );
        gauge
    }

    /// Обновляет значение из `/proc/self/status`; `None` — источник недоступен (не Linux).
    pub async fn update(&self) -> Option<u64> {
        let status = tokio::fs::read_to_string("/proc/self/status").await.ok()?;
        let rss = parse_vm_rss(&status)?;
        self.0.set(i64::try_from(rss).unwrap_or(i64::MAX));
        Some(rss)
    }

    /// Текущее значение метрики, байт.
    pub fn get(&self) -> i64 {
        self.0.get()
    }

    /// Обновляет метрику каждые `cfg.rss_interval` до сигнала остановки.
    pub async fn run(self, cfg: &RuntimeConfig, shutdown: CancellationToken) {
        if cfg.rss_interval.is_zero() {
            return;
        }
        let mut tick = tokio::time::interval(cfg.rss_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = tick.tick() => {
                    self.update().await;
                }
            }
        }
    }
}

/// `VmRSS:  1234 kB` из `/proc/<pid>/status` → байты.
fn parse_vm_rss(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let mut parts = line["VmRSS:".len()..].split_whitespace();
    let value: u64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some("kB") | None => value.checked_mul(1024),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use tp_contracts::role::{RoleEntry, RolePreset, fixed_config};

    use super::*;
    use crate::config::SectionRegistry;

    const GIB: u64 = 1 << 30;

    fn node(cpus: f64, memory: u64) -> NodeResources {
        NodeResources {
            cpus,
            memory_total: memory,
            memory_available: memory / 2,
        }
    }

    /// Поведение тестовой роли после сигнала остановки.
    enum Mode {
        /// Завершает работу за указанное время.
        Drain(Duration),
        /// Игнорирует сигнал.
        Hang,
        /// Каждый запуск сразу завершается ошибкой.
        Fail,
        /// Первые `n` запусков завершаются ошибкой, затем работает до сигнала.
        FailFirst(u32),
        /// Первый запуск паникует, затем работает до сигнала.
        PanicOnce,
    }

    struct TestRole {
        name: &'static str,
        mode: Mode,
        dropped: Arc<AtomicBool>,
        runs: Arc<AtomicU32>,
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    // Трейт объявлен через `async_trait`; реализация — его развёрткой (крейт не зависит от `async-trait`).
    impl RoleService for TestRole {
        fn name(&self) -> &str {
            self.name
        }

        fn run<'life0, 'async_trait>(
            &'life0 self,
            ctx: RoleCtx,
        ) -> Pin<Box<dyn Future<Output = tp_contracts::Result<()>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                let _flag = DropFlag(self.dropped.clone());
                let run = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
                match self.mode {
                    Mode::Fail => return Err(Error::internal("boom")),
                    Mode::FailFirst(n) if run <= n => return Err(Error::internal("boom")),
                    Mode::PanicOnce if run == 1 => panic!("role panic"),
                    _ => {}
                }
                ctx.ready.set(true);
                ctx.shutdown.cancelled().await;
                match self.mode {
                    Mode::Drain(d) => tokio::time::sleep(d).await,
                    Mode::Hang => std::future::pending::<()>().await,
                    _ => {}
                }
                Ok(())
            })
        }
    }

    fn spec(name: &'static str, mode: Mode) -> (RoleSpec, Arc<AtomicBool>) {
        let (spec, dropped, _) = spec_runs(name, mode);
        (spec, dropped)
    }

    fn spec_runs(name: &'static str, mode: Mode) -> (RoleSpec, Arc<AtomicBool>, Arc<AtomicU32>) {
        let dropped = Arc::new(AtomicBool::new(false));
        let runs = Arc::new(AtomicU32::new(0));
        let spec = RoleSpec {
            service: Arc::new(TestRole {
                name,
                mode,
                dropped: dropped.clone(),
                runs: runs.clone(),
            }),
            config: fixed_config(()),
        };
        (spec, dropped, runs)
    }

    /// Перезапуск с паузой 10–40 мс.
    fn fast_restart(max_restarts: u32) -> RestartConfig {
        RestartConfig {
            max_restarts,
            window: Duration::from_secs(600),
            backoff_initial: Duration::from_millis(10),
            backoff_max: Duration::from_millis(40),
        }
    }

    fn runtime(drain_ms: u64, roles: &[Role]) -> Roles {
        runtime_with(drain_ms, fast_restart(5), roles)
    }

    fn runtime_with(drain_ms: u64, restart: RestartConfig, roles: &[Role]) -> Roles {
        let cfg = RuntimeConfig {
            drain_deadline: Duration::from_millis(drain_ms),
            restart,
            ..RuntimeConfig::default()
        };
        let set = RoleSet::expand(
            &roles
                .iter()
                .map(|r| RoleEntry::Role(*r))
                .collect::<Vec<_>>(),
        );
        Roles::new(cfg, set, &node(4.0, 8 * GIB))
    }

    async fn wait_ready(health: &Health) {
        while !health.is_ready() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[test]
    fn limits_follow_node_resources_and_role_set() {
        let cfg = RuntimeConfig::default();
        let all = RoleSet::expand(&[RoleEntry::Preset(RolePreset::All)]);
        let limits = role_limits(&cfg, &all, &node(4.0, 10 * GIB));
        assert_eq!(limits.len(), Role::ALL.len());
        // 25 % от 10 GiB на 10 ролей; пул min(4 CPU × 4, 64) = 16 на 10 ролей → 1.
        let imap = limits[&Role::Imap];
        assert_eq!(imap.cache_bytes, 10 * GIB / 100 * 25 / 10);
        assert_eq!(imap.db_pool, 1);

        let edge = RoleSet::expand(&[RoleEntry::Preset(RolePreset::Edge)]);
        let limits = role_limits(&cfg, &edge, &node(1.5, 2 * GIB));
        assert_eq!(limits.keys().copied().collect::<Vec<_>>(), [Role::SmtpIn]);
        assert_eq!(limits[&Role::SmtpIn].cache_bytes, 2 * GIB / 100 * 25);
        assert_eq!(limits[&Role::SmtpIn].db_pool, 8, "ceil(1.5) CPU × 4");

        let mut cfg = RuntimeConfig {
            db_pool_max: 6,
            ..RuntimeConfig::default()
        };
        cfg.limits.insert(
            Role::Search,
            RoleLimitsOverride {
                cache_bytes: Some(GIB),
                db_pool: None,
            },
        );
        let backend = RoleSet::expand(&[RoleEntry::Preset(RolePreset::Backend)]);
        let limits = role_limits(&cfg, &backend, &node(64.0, 4 * GIB));
        assert_eq!(limits[&Role::Search].cache_bytes, GIB);
        assert_eq!(limits[&Role::Search].db_pool, 2, "max 6 on 3 roles");
        assert_eq!(limits[&Role::Indexer].cache_bytes, 4 * GIB / 100 * 25 / 3);
    }

    #[test]
    fn config_parses_role_overrides() {
        let cfg: RuntimeConfig = serde_yaml_ng::from_str(
            "drain_deadline: 500ms\nlimits:\n  search: { cache_bytes: 1024 }\n",
        )
        .expect("parse");
        assert_eq!(cfg.drain_deadline, Duration::from_millis(500));
        assert_eq!(cfg.limits[&Role::Search].cache_bytes, Some(1024));
        assert_eq!(cfg.cache_memory_percent, 25);
        assert!(serde_yaml_ng::from_str::<RuntimeConfig>("limits: { bogus: {} }").is_err());
        assert_eq!(cfg.rss_interval, Duration::from_secs(15));
        let off: RuntimeConfig = serde_yaml_ng::from_str("rss_interval: 0s").expect("0s");
        assert!(off.rss_interval.is_zero());
        assert!(serde_yaml_ng::from_str::<RuntimeConfig>("drain_deadline: 500").is_err());
        assert!(serde_yaml_ng::from_str::<RuntimeConfig>("drain_deadline_ms: 500").is_err());
    }

    #[test]
    fn disabled_role_is_not_constructed() {
        let mut roles = runtime(100, &[Role::Imap]);
        let built = AtomicBool::new(false);
        let added = roles.add(Role::Search, |_| {
            built.store(true, Ordering::SeqCst);
            spec("search", Mode::Hang).0
        });
        assert!(!added);
        assert!(!built.load(Ordering::SeqCst));
        assert_eq!(roles.limits(Role::Search), None);
        assert!(roles.add(Role::Imap, |limits| {
            assert!(limits.cache_bytes > 0 && limits.db_pool > 0);
            spec("imap", Mode::Drain(Duration::ZERO)).0
        }));
        assert_eq!(roles.names(), ["imap"]);
    }

    #[tokio::test]
    async fn roles_drain_within_deadline() {
        let mut roles = runtime(2_000, &[Role::Imap, Role::Jmap]);
        let (imap, imap_dropped) = spec("imap", Mode::Drain(Duration::from_millis(50)));
        roles.add(Role::Imap, |_| imap);
        roles.add(Role::Jmap, |_| spec("jmap", Mode::Drain(Duration::ZERO)).0);
        let health = Health::default();
        let metrics = Metrics::default();
        let shutdown = CancellationToken::new();
        let trigger = shutdown.clone();
        let run = tokio::spawn({
            let health = health.clone();
            let metrics = metrics.clone();
            async move { roles.run(&health, &metrics, shutdown).await }
        });
        wait_ready(&health).await;
        assert!(
            metrics
                .encode()
                .expect("encode")
                .contains("tp_roles_running 2")
        );

        let started = Instant::now();
        trigger.cancel();
        let report = run.await.expect("join");
        assert!(started.elapsed() < Duration::from_millis(2_000));
        assert!(health.is_draining());
        assert!(!health.is_ready());
        let mut completed = report.completed.clone();
        completed.sort();
        assert_eq!(completed, ["imap", "jmap"]);
        assert!(report.cancelled.is_empty() && report.failed.is_empty());
        assert!(imap_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn hung_role_is_cancelled_after_deadline() {
        let mut roles = runtime(100, &[Role::Imap, Role::Dav]);
        let (hung, hung_dropped) = spec("imap", Mode::Hang);
        roles.add(Role::Imap, |_| hung);
        roles.add(Role::Dav, |_| spec("dav", Mode::Drain(Duration::ZERO)).0);
        let health = Health::default();
        let shutdown = CancellationToken::new();
        let trigger = shutdown.clone();
        let run = tokio::spawn({
            let health = health.clone();
            async move { roles.run(&health, &Metrics::default(), shutdown).await }
        });
        wait_ready(&health).await;

        let started = Instant::now();
        trigger.cancel();
        let report = run.await.expect("join");
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(100), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
        assert_eq!(report.completed, ["dav"]);
        assert_eq!(report.cancelled, ["imap"]);
        assert!(hung_dropped.load(Ordering::SeqCst), "hung task dropped");
    }

    #[test]
    fn restart_backoff_doubles_up_to_limit() {
        let cfg = RestartConfig::default();
        let ms = |n| cfg.backoff(n).as_millis();
        assert_eq!(
            [ms(1), ms(2), ms(3), ms(6), ms(7), ms(100)],
            [1_000, 2_000, 4_000, 32_000, 60_000, 60_000]
        );

        let cfg: RuntimeConfig =
            serde_yaml_ng::from_str("restart: { max_restarts: 0, backoff_max: 500ms }\n")
                .expect("parse");
        assert_eq!(cfg.restart.max_restarts, 0);
        assert_eq!(cfg.restart.backoff(10), Duration::from_millis(500));
        assert_eq!(cfg.restart.window, Duration::from_secs(600));
        assert!(serde_yaml_ng::from_str::<RuntimeConfig>("restart: { bogus: 1 }").is_err());
    }

    #[test]
    fn failures_outside_window_are_forgotten() {
        let (spec, _) = spec("imap", Mode::Fail);
        let mut slot = Slot {
            name: "imap".to_owned(),
            service: spec.service,
            config: spec.config,
            ready: Health::default().register("imap"),
            failures: VecDeque::new(),
        };
        let t0 = Instant::now();
        let window = Duration::from_secs(60);
        assert_eq!(slot.fail(t0, window), 1);
        assert_eq!(slot.fail(t0 + Duration::from_secs(30), window), 2);
        assert_eq!(
            slot.fail(t0 + Duration::from_secs(61), window),
            2,
            "first one expired"
        );
        assert_eq!(slot.fail(t0 + Duration::from_secs(200), window), 1);
    }

    #[tokio::test]
    async fn failed_role_is_restarted_others_keep_running() {
        let mut roles = runtime(1_000, &[Role::Admin, Role::Maintenance]);
        let (admin, _, admin_runs) = spec_runs("admin", Mode::FailFirst(2));
        roles.add(Role::Admin, |_| admin);
        let (maint, maint_dropped, maint_runs) =
            spec_runs("maintenance", Mode::Drain(Duration::ZERO));
        roles.add(Role::Maintenance, |_| maint);
        let health = Health::default();
        let metrics = Metrics::default();
        let shutdown = CancellationToken::new();
        let trigger = shutdown.clone();
        let run = tokio::spawn({
            let health = health.clone();
            let metrics = metrics.clone();
            async move { roles.run(&health, &metrics, shutdown).await }
        });
        tokio::time::timeout(Duration::from_secs(5), wait_ready(&health))
            .await
            .expect("admin is back");
        assert_eq!(admin_runs.load(Ordering::SeqCst), 3);
        assert_eq!(
            maint_runs.load(Ordering::SeqCst),
            1,
            "neighbour not restarted"
        );
        assert!(
            !maint_dropped.load(Ordering::SeqCst),
            "neighbour still running"
        );
        let text = metrics.encode().expect("encode");
        assert!(
            text.contains("tp_role_restarts_total{role=\"admin\"} 2"),
            "{text}"
        );

        trigger.cancel();
        let report = run.await.expect("join");
        assert_eq!(report.restarts, BTreeMap::from([("admin".to_owned(), 2)]));
        assert!(report.failed.is_empty());
        let mut completed = report.completed.clone();
        completed.sort();
        assert_eq!(completed, ["admin", "maintenance"]);
    }

    #[tokio::test]
    async fn panicking_role_is_restarted() {
        let mut roles = runtime(1_000, &[Role::Imap]);
        let (imap, _, runs) = spec_runs("imap", Mode::PanicOnce);
        roles.add(Role::Imap, |_| imap);
        let health = Health::default();
        let shutdown = CancellationToken::new();
        let trigger = shutdown.clone();
        let run = tokio::spawn({
            let health = health.clone();
            async move { roles.run(&health, &Metrics::default(), shutdown).await }
        });
        tokio::time::timeout(Duration::from_secs(5), wait_ready(&health))
            .await
            .expect("imap is back");
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        trigger.cancel();
        let report = run.await.expect("join");
        assert_eq!(report.restarts["imap"], 1);
        assert_eq!(report.completed, ["imap"]);
    }

    #[tokio::test]
    async fn restart_limit_stops_node() {
        let mut roles = runtime_with(1_000, fast_restart(2), &[Role::Admin, Role::Maintenance]);
        let (admin, _, admin_runs) = spec_runs("admin", Mode::Fail);
        roles.add(Role::Admin, |_| admin);
        let (maint, maint_dropped) = spec("maintenance", Mode::Drain(Duration::ZERO));
        roles.add(Role::Maintenance, |_| maint);
        let health = Health::default();
        let shutdown = CancellationToken::new();
        let report = tokio::time::timeout(
            Duration::from_secs(5),
            roles.run(&health, &Metrics::default(), shutdown),
        )
        .await
        .expect("node stops by itself");
        assert_eq!(admin_runs.load(Ordering::SeqCst), 3, "start + 2 restarts");
        assert_eq!(report.restarts["admin"], 2);
        assert_eq!(
            report.failed,
            [("admin".to_owned(), Error::internal("boom"))]
        );
        assert_eq!(report.completed, ["maintenance"]);
        assert!(maint_dropped.load(Ordering::SeqCst));
        assert!(health.is_draining());
    }

    #[tokio::test]
    async fn zero_restarts_means_fail_fast() {
        let mut roles = runtime_with(1_000, fast_restart(0), &[Role::Admin]);
        let (admin, _, runs) = spec_runs("admin", Mode::Fail);
        roles.add(Role::Admin, |_| admin);
        let shutdown = CancellationToken::new();
        let report = tokio::time::timeout(
            Duration::from_secs(5),
            roles.run(&Health::default(), &Metrics::default(), shutdown),
        )
        .await
        .expect("node stops by itself");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(report.restarts.is_empty());
        assert_eq!(report.failed.len(), 1);
    }

    #[tokio::test]
    async fn shutdown_during_backoff_skips_restart() {
        let restart = RestartConfig {
            backoff_initial: Duration::from_secs(60),
            ..fast_restart(5)
        };
        let mut roles = runtime_with(1_000, restart, &[Role::Admin, Role::Maintenance]);
        let (admin, _, admin_runs) = spec_runs("admin", Mode::Fail);
        roles.add(Role::Admin, |_| admin);
        roles.add(Role::Maintenance, |_| {
            spec("maintenance", Mode::Drain(Duration::ZERO)).0
        });
        let health = Health::default();
        let metrics = Metrics::default();
        let shutdown = CancellationToken::new();
        let trigger = shutdown.clone();
        let run = tokio::spawn({
            let health = health.clone();
            let metrics = metrics.clone();
            async move { roles.run(&health, &metrics, shutdown).await }
        });
        while !metrics
            .encode()
            .expect("encode")
            .contains("tp_role_restarts_total")
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !health.is_ready(),
            "failed role is not ready during backoff"
        );
        let started = Instant::now();
        trigger.cancel();
        let report = run.await.expect("join");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(admin_runs.load(Ordering::SeqCst), 1);
        assert!(report.failed.is_empty() && report.cancelled.is_empty());
    }

    #[tokio::test]
    async fn restart_settings_follow_config_reload() {
        let registry = SectionRegistry::new();
        let base = "database: { url: pg }\nblob: { url: file:///b }\ndomain: d\n";
        let parse = |extra: &str| {
            Arc::new(
                registry
                    .parse(&format!("{base}{extra}"), Vec::new())
                    .expect("parse"),
            )
        };
        let (tx, rx) = watch::channel(parse(""));
        // Сначала лимит 5; после перечитывания — 0, и следующее падение останавливает узел.
        let mut roles = runtime_with(1_000, fast_restart(5), &[Role::Admin]).with_updates(rx);
        let (admin, _, runs) = spec_runs("admin", Mode::Fail);
        roles.add(Role::Admin, |_| admin);
        tx.send(parse("runtime: { restart: { max_restarts: 0 } }\n"))
            .expect("send");
        let shutdown = CancellationToken::new();
        let report = tokio::time::timeout(
            Duration::from_secs(5),
            roles.run(&Health::default(), &Metrics::default(), shutdown),
        )
        .await
        .expect("node stops by itself");
        assert_eq!(report.failed.len(), 1);
        assert!(runs.load(Ordering::SeqCst) <= 2, "new limit applied");
    }

    #[test]
    fn vm_rss_parses() {
        let status = "Name:\tthepost\nVmPeak:\t  9000 kB\nVmRSS:\t   5120 kB\nThreads:\t4\n";
        assert_eq!(parse_vm_rss(status), Some(5120 * 1024));
        assert_eq!(parse_vm_rss("Name: x\n"), None);
        assert_eq!(parse_vm_rss("VmRSS: lots\n"), None);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn rss_metric_is_exported() {
        let metrics = Metrics::default();
        let rss = RssGauge::register(&metrics);
        let value = rss.update().await.expect("procfs");
        assert!(value > 0);
        assert_eq!(rss.get(), i64::try_from(value).expect("fits"));
        let text = metrics.encode().expect("encode");
        assert!(
            text.contains("# TYPE tp_process_resident_memory_bytes gauge"),
            "{text}"
        );
    }
}
