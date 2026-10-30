//! Рантайм ролей (SRS 2.1, 2.7, CONTRACTS.md#Roles): запуск набора [`RoleService`] по конфигурации,
//! лимиты кэшей и пулов по ролям от ресурсов узла, штатная остановка с drain, метрика RSS процесса.
//!
//! Ленивая инициализация: сервис роли создаётся фабрикой только для включённой роли, поэтому невключённая
//! роль не создаёт пулов, кэшей, потоков и портов. Остановка: сигнал → узел выводится из пула (`/healthz/ready`
//! 503) → роли получают `ctx.shutdown` и завершают работу за `drain_deadline` → оставшиеся задачи отменяются.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use prometheus_client::metrics::gauge::Gauge;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::task::{Id, JoinSet};
use tp_contracts::Error;
use tp_contracts::role::{CancellationToken, ConfigRx, Role, RoleCtx, RoleService, RoleSet};
use tp_sys::NodeResources;

use crate::service_http::Health;
use crate::telemetry::Metrics;

/// Секция `runtime`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    /// Время на завершение текущей работы ролей после сигнала остановки, мс; затем задачи отменяются.
    pub drain_deadline_ms: u64,
    /// Доля памяти узла под кэши всех включённых ролей, %. Делится поровну между ролями. Требует перезапуска.
    pub cache_memory_percent: u8,
    /// Соединений с PostgreSQL на один CPU узла. Делятся поровну между ролями. Требует перезапуска.
    pub db_pool_per_cpu: u32,
    /// Предел соединений с PostgreSQL на узел. Требует перезапуска.
    pub db_pool_max: u32,
    /// Явные лимиты отдельных ролей вместо рассчитанных. Требует перезапуска.
    pub limits: BTreeMap<Role, RoleLimitsOverride>,
    /// Период обновления метрики RSS процесса, мс; 0 — метрика не обновляется.
    pub rss_interval_ms: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            drain_deadline_ms: 30_000,
            cache_memory_percent: 25,
            db_pool_per_cpu: 4,
            db_pool_max: 64,
            limits: BTreeMap::new(),
            rss_interval_ms: 15_000,
        }
    }
}

impl RuntimeConfig {
    /// `drain_deadline_ms` как `Duration`.
    pub fn drain_deadline(&self) -> Duration {
        Duration::from_millis(self.drain_deadline_ms)
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
    /// Роли, завершившиеся ошибкой или паникой.
    pub failed: Vec<(String, Error)>,
}

/// Набор ролей узла до запуска.
pub struct Roles {
    cfg: RuntimeConfig,
    set: RoleSet,
    limits: BTreeMap<Role, RoleLimits>,
    specs: Vec<RoleSpec>,
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
        }
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

    /// Запускает роли и работает до сигнала `shutdown` или аварийного завершения роли, затем останавливает все роли.
    ///
    /// Аварийное завершение роли останавливает узел: остальные роли проходят drain, ошибка попадает в отчёт.
    pub async fn run(
        self,
        health: &Health,
        metrics: &Metrics,
        shutdown: CancellationToken,
    ) -> RolesReport {
        let deadline = self.cfg.drain_deadline();
        let running = Gauge::<i64>::default();
        metrics.register("tp_roles_running", "Running role tasks", running.clone());

        let role_shutdown = CancellationToken::new();
        let mut tasks = JoinSet::new();
        let mut names: BTreeMap<Id, String> = BTreeMap::new();
        let mut handles = Vec::new();
        for spec in self.specs {
            let name = spec.service.name().to_owned();
            let ready = health.register(&name);
            handles.push(ready.clone());
            let ctx = RoleCtx {
                config: spec.config,
                shutdown: role_shutdown.clone(),
                drain_deadline: deadline,
                ready,
            };
            let service = spec.service;
            let handle = tasks.spawn(async move { service.run(ctx).await });
            names.insert(handle.id(), name);
        }
        running.set(tasks.len() as i64);
        tracing::info!(roles = ?names.values().collect::<Vec<_>>(), "roles started");

        let mut report = RolesReport::default();
        let name_of =
            |names: &BTreeMap<Id, String>, id| names.get(&id).cloned().unwrap_or_default();
        // До сигнала: роль, завершившаяся сама, — штатно (Ok) или аварийно (ошибка → остановка узла).
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                done = tasks.join_next_with_id() => {
                    let Some(done) = done else { break };
                    running.set(tasks.len() as i64);
                    let failed = record(&mut report, done, &names, name_of);
                    if failed {
                        tracing::error!("role failed, stopping node");
                        break;
                    }
                }
            }
        }

        tracing::info!(deadline_ms = deadline.as_millis() as u64, "roles draining");
        health.drain();
        for h in &handles {
            h.set(false);
        }
        role_shutdown.cancel();

        let drained = tokio::time::timeout(deadline, async {
            while let Some(done) = tasks.join_next_with_id().await {
                running.set(tasks.len() as i64);
                record(&mut report, done, &names, name_of);
            }
        })
        .await;
        if drained.is_err() {
            tasks.abort_all();
            while let Some(done) = tasks.join_next_with_id().await {
                match done {
                    Err(e) if e.is_cancelled() => {
                        let name = name_of(&names, e.id());
                        tracing::warn!(role = %name, "role cancelled after drain deadline");
                        report.cancelled.push(name);
                    }
                    done => {
                        record(&mut report, done, &names, name_of);
                    }
                }
            }
        }
        running.set(0);
        report
    }
}

/// Учитывает завершённую задачу роли; `true` — аварийное завершение.
fn record(
    report: &mut RolesReport,
    done: Result<(Id, tp_contracts::Result<()>), tokio::task::JoinError>,
    names: &BTreeMap<Id, String>,
    name_of: impl Fn(&BTreeMap<Id, String>, Id) -> String,
) -> bool {
    match done {
        Ok((id, Ok(()))) => {
            report.completed.push(name_of(names, id));
            false
        }
        Ok((id, Err(e))) => {
            let name = name_of(names, id);
            tracing::error!(role = %name, error = %e, "role failed");
            report.failed.push((name, e));
            true
        }
        Err(e) => {
            let name = name_of(names, e.id());
            let err = if e.is_panic() {
                Error::internal("role panicked")
            } else {
                Error::internal("role task cancelled")
            };
            tracing::error!(role = %name, error = %err, "role task ended abnormally");
            report.failed.push((name, err));
            true
        }
    }
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

    /// Обновляет метрику каждые `cfg.rss_interval_ms` до сигнала остановки.
    pub async fn run(self, cfg: &RuntimeConfig, shutdown: CancellationToken) {
        if cfg.rss_interval_ms == 0 {
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_millis(cfg.rss_interval_ms));
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use tp_contracts::role::{RoleEntry, RolePreset, fixed_config};

    use super::*;

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
        /// Сразу завершается ошибкой.
        Fail,
    }

    struct TestRole {
        name: &'static str,
        mode: Mode,
        dropped: Arc<AtomicBool>,
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
                if matches!(self.mode, Mode::Fail) {
                    return Err(Error::internal("boom"));
                }
                ctx.ready.set(true);
                ctx.shutdown.cancelled().await;
                match self.mode {
                    Mode::Drain(d) => tokio::time::sleep(d).await,
                    Mode::Hang => std::future::pending::<()>().await,
                    Mode::Fail => {}
                }
                Ok(())
            })
        }
    }

    fn spec(name: &'static str, mode: Mode) -> (RoleSpec, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        let spec = RoleSpec {
            service: Arc::new(TestRole {
                name,
                mode,
                dropped: dropped.clone(),
            }),
            config: fixed_config(()),
        };
        (spec, dropped)
    }

    fn runtime(drain_ms: u64, roles: &[Role]) -> Roles {
        let cfg = RuntimeConfig {
            drain_deadline_ms: drain_ms,
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
            "drain_deadline_ms: 500\nlimits:\n  search: { cache_bytes: 1024 }\n",
        )
        .expect("parse");
        assert_eq!(cfg.drain_deadline(), Duration::from_millis(500));
        assert_eq!(cfg.limits[&Role::Search].cache_bytes, Some(1024));
        assert_eq!(cfg.cache_memory_percent, 25);
        assert!(serde_yaml_ng::from_str::<RuntimeConfig>("limits: { bogus: {} }").is_err());
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

    #[tokio::test]
    async fn failed_role_stops_node() {
        let mut roles = runtime(1_000, &[Role::Admin, Role::Maintenance]);
        roles.add(Role::Admin, |_| spec("admin", Mode::Fail).0);
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
        assert_eq!(
            report.failed,
            [("admin".to_owned(), Error::internal("boom"))]
        );
        assert_eq!(report.completed, ["maintenance"]);
        assert!(maint_dropped.load(Ordering::SeqCst));
        assert!(health.is_draining());
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
