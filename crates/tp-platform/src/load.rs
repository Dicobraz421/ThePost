//! Поведение под нагрузкой (SRS 6.3, CONTRACTS.md#Load; решение 2026-11-01-borrowings-security): монитор уровня
//! нагрузки узла [`NodeLoad`] — реализация [`LoadMonitor`].
//!
//! Показатели — доли от текущих лимитов: CPU и память относительно лимитов cgroups (снимки SYS-06
//! [`tp_sys::watch_resources`]), занятость дисков кэша, пула PostgreSQL, длина очередей и задержка записи (их
//! передают владельцы через [`LoadInputs`]). Уровень — по наибольшему показателю: `elevated_percent` (85 %) —
//! [`LoadLevel::Elevated`], `high_percent` (95 %) — [`LoadLevel::High`]; снижение уровня — только ниже порога на
//! `hysteresis_percent`. Пересчёт — по событию: новый снимок ресурсов (в т.ч. новые лимиты cgroups), новое значение
//! показателя, новая секция `load`; своего таймера нет (SRS 2.8).
//!
//! Метрики: `tp_load_level` (0 — normal, 1 — elevated, 2 — high), `tp_load_transitions_total{to}`,
//! `tp_load_ratio{indicator}`; для автомасштабирования, которое реагирует раньше ограничения потока, —
//! `tp_load_connections`, `tp_load_queue_length`, `tp_load_scale_wanted` (наибольший показатель не ниже
//! `scale_percent`).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::watch;
use tp_contracts::Error;
use tp_contracts::config::duration;
use tp_contracts::load::{LoadLevel, LoadMonitor, LoadRx};
use tp_contracts::role::{CancellationToken, RoleCtx};
use tp_contracts::tls::Profile;
use tp_sys::ResourceSnapshot;

use crate::config::Config;
use crate::telemetry::Metrics;

/// Имя секции.
pub const LOAD: &str = "load";

/// Секция `load`: пороги уровней нагрузки (SRS 6.3). Применяется на ходу.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LoadConfig {
    /// Порог уровня «повышенный», % от лимита по наибольшему показателю: меньше параллельность новых сессий,
    /// фоновые задания приостановлены.
    pub elevated_percent: u8,
    /// Порог уровня «высокий», %: новым сессиям SMTP — 4xx, новые сессии IMAP и JMAP ограничены.
    pub high_percent: u8,
    /// Гистерезис, процентных пунктов: уровень снижается, когда показатель ниже порога на эту величину.
    pub hysteresis_percent: u8,
    /// Порог масштабирования, %: метрика `tp_load_scale_wanted`; ниже порогов ограничения — автомасштабирование
    /// реагирует первым.
    pub scale_percent: u8,
    /// Длина очередей, принимаемая за 100 %; 0 — показатель не учитывается.
    pub queue_capacity: u64,
    /// Задержка записи, принимаемая за 100 % (`1s`); `0s` — показатель не учитывается.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub write_latency_max: Duration,
}

impl Default for LoadConfig {
    fn default() -> Self {
        Self {
            elevated_percent: 85,
            high_percent: 95,
            hysteresis_percent: 5,
            scale_percent: 70,
            queue_capacity: 10_000,
            write_latency_max: Duration::from_secs(1),
        }
    }
}

impl LoadConfig {
    /// Проверка секции: `scale_percent ≤ elevated_percent < high_percent ≤ 100`.
    pub fn check(&self, _profile: Profile) -> tp_contracts::Result<()> {
        if self.high_percent > 100
            || self.elevated_percent >= self.high_percent
            || self.scale_percent > self.elevated_percent
        {
            return Err(Error::Invalid(format!(
                "{LOAD}: required scale_percent <= elevated_percent < high_percent <= 100, got {} / {} / {}",
                self.scale_percent, self.elevated_percent, self.high_percent
            )));
        }
        Ok(())
    }

    /// Следующий уровень после `current` при наибольшем показателе `peak` (доля, 1.0 — 100 %) с гистерезисом.
    pub fn next_level(&self, current: LoadLevel, peak: f64) -> LoadLevel {
        // Допуск на погрешность f64: 0.85 × 100 — уже порог 85 %.
        let pct = peak * 100.0 + 1e-9;
        let hyst = f64::from(self.hysteresis_percent);
        let high = f64::from(self.high_percent);
        let elevated = f64::from(self.elevated_percent);
        let holds = |level: LoadLevel, threshold: f64| {
            pct >= threshold || (current >= level && pct >= threshold - hyst)
        };
        if holds(LoadLevel::High, high) {
            LoadLevel::High
        } else if holds(LoadLevel::Elevated, elevated) {
            LoadLevel::Elevated
        } else {
            LoadLevel::Normal
        }
    }
}

/// Контролируемый показатель (SRS 6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Indicator {
    /// CPU относительно квоты cgroup.
    Cpu,
    /// Память относительно `memory.max` cgroup.
    Memory,
    /// Занятое место на дисках кэша.
    CacheDisk,
    /// Занятые соединения пула PostgreSQL.
    DbPool,
    /// Длина очередей относительно `queue_capacity`.
    Queues,
    /// Задержка записи относительно `write_latency_max`.
    WriteLatency,
}

impl Indicator {
    /// Все показатели.
    pub const ALL: &'static [Indicator] = &[
        Indicator::Cpu,
        Indicator::Memory,
        Indicator::CacheDisk,
        Indicator::DbPool,
        Indicator::Queues,
        Indicator::WriteLatency,
    ];

    /// Метка метрик и журнала.
    pub const fn as_str(self) -> &'static str {
        match self {
            Indicator::Cpu => "cpu",
            Indicator::Memory => "memory",
            Indicator::CacheDisk => "cache_disk",
            Indicator::DbPool => "db_pool",
            Indicator::Queues => "queues",
            Indicator::WriteLatency => "write_latency",
        }
    }
}

impl fmt::Display for Indicator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Показатели от владельцев: пул БД, очереди, кэш, запись; `None` — источник не сообщал значения.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Inputs {
    /// Свободно и всего байт на дисках кэша.
    pub cache_disk: Option<(u64, u64)>,
    /// Занято и всего соединений пула PostgreSQL.
    pub db_pool: Option<(u32, u32)>,
    /// Суммарная длина очередей.
    pub queue_len: Option<u64>,
    /// Задержка записи.
    pub write_latency: Option<Duration>,
    /// Открытых клиентских соединений (метрика автомасштабирования, на уровень не влияет).
    pub connections: Option<u64>,
}

/// Вход показателей монитора: клоны разделяют значения; каждое изменение пересчитывает уровень.
#[derive(Debug, Clone)]
pub struct LoadInputs(Arc<watch::Sender<Inputs>>);

impl Default for LoadInputs {
    fn default() -> Self {
        Self(Arc::new(watch::channel(Inputs::default()).0))
    }
}

impl LoadInputs {
    fn update(&self, f: impl FnOnce(&mut Inputs)) {
        self.0.send_if_modified(|cur| {
            let old = *cur;
            f(cur);
            *cur != old
        });
    }

    /// Диски кэша: свободно и всего байт.
    pub fn set_cache_disk(&self, free: u64, total: u64) {
        self.update(|i| i.cache_disk = Some((free, total)));
    }

    /// Пул PostgreSQL: занято и всего соединений.
    pub fn set_db_pool(&self, in_use: u32, size: u32) {
        self.update(|i| i.db_pool = Some((in_use, size)));
    }

    /// Суммарная длина очередей.
    pub fn set_queue_len(&self, len: u64) {
        self.update(|i| i.queue_len = Some(len));
    }

    /// Задержка записи (последняя оценка владельца, например p99 за окно).
    pub fn set_write_latency(&self, latency: Duration) {
        self.update(|i| i.write_latency = Some(latency));
    }

    /// Открытых клиентских соединений.
    pub fn set_connections(&self, n: u64) {
        self.update(|i| i.connections = Some(n));
    }

    /// Текущие значения.
    pub fn get(&self) -> Inputs {
        *self.0.borrow()
    }
}

/// Доли показателей (1.0 — 100 % лимита); отсутствующий источник в результат не входит.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Readings(pub BTreeMap<Indicator, f64>);

impl Readings {
    /// Доли по снимку ресурсов и показателям владельцев.
    pub fn evaluate(cfg: &LoadConfig, snap: &ResourceSnapshot, inputs: &Inputs) -> Self {
        let mut out = BTreeMap::new();
        let ratio = |used: f64, limit: f64| (limit > 0.0).then(|| (used / limit).max(0.0));
        let res = &snap.resources;
        if let Some(r) = snap.cpu_used.and_then(|used| ratio(used, res.cpus)) {
            out.insert(Indicator::Cpu, r);
        }
        if res.memory_total != u64::MAX {
            let used = res.memory_total.saturating_sub(res.memory_available);
            if let Some(r) = ratio(used as f64, res.memory_total as f64) {
                out.insert(Indicator::Memory, r);
            }
        }
        if let Some(r) = inputs
            .cache_disk
            .and_then(|(free, total)| ratio(total.saturating_sub(free) as f64, total as f64))
        {
            out.insert(Indicator::CacheDisk, r);
        }
        if let Some(r) = inputs
            .db_pool
            .and_then(|(used, size)| ratio(f64::from(used), f64::from(size)))
        {
            out.insert(Indicator::DbPool, r);
        }
        if let Some(r) = inputs
            .queue_len
            .and_then(|len| ratio(len as f64, cfg.queue_capacity as f64))
        {
            out.insert(Indicator::Queues, r);
        }
        if let Some(r) = inputs
            .write_latency
            .and_then(|d| ratio(d.as_secs_f64(), cfg.write_latency_max.as_secs_f64()))
        {
            out.insert(Indicator::WriteLatency, r);
        }
        Self(out)
    }

    /// Наибольший показатель; без показателей — `(Cpu, 0.0)`.
    pub fn peak(&self) -> (Indicator, f64) {
        self.0
            .iter()
            .map(|(i, r)| (*i, *r))
            .fold((Indicator::Cpu, 0.0), |a, b| if b.1 > a.1 { b } else { a })
    }
}

/// Метрики монитора.
#[derive(Debug, Clone, Default)]
struct LoadMetrics {
    level: Gauge,
    transitions: Family<Vec<(String, String)>, Counter>,
    ratio: Family<Vec<(String, String)>, Gauge<f64, AtomicU64>>,
    connections: Gauge,
    queue_length: Gauge,
    scale_wanted: Gauge,
}

impl LoadMetrics {
    fn register(metrics: &Metrics) -> Self {
        let m = Self::default();
        metrics.register(
            "tp_load_level",
            "Node load level: 0 normal, 1 elevated, 2 high",
            m.level.clone(),
        );
        metrics.register(
            "tp_load_transitions",
            "Node load level transitions",
            m.transitions.clone(),
        );
        metrics.register(
            "tp_load_ratio",
            "Load indicator relative to its current limit (1.0 = 100%)",
            m.ratio.clone(),
        );
        metrics.register(
            "tp_load_connections",
            "Open client connections (autoscaling)",
            m.connections.clone(),
        );
        metrics.register(
            "tp_load_queue_length",
            "Total queue length (autoscaling)",
            m.queue_length.clone(),
        );
        metrics.register(
            "tp_load_scale_wanted",
            "Peak load indicator is above the scaling threshold (autoscaling)",
            m.scale_wanted.clone(),
        );
        m
    }

    fn observe(&self, cfg: &LoadConfig, level: LoadLevel, readings: &Readings, inputs: &Inputs) {
        self.level.set(level_value(level));
        for (indicator, r) in &readings.0 {
            self.ratio
                .get_or_create(&vec![(
                    "indicator".to_owned(),
                    indicator.as_str().to_owned(),
                )])
                .set(*r);
        }
        let as_i64 = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        if let Some(n) = inputs.connections {
            self.connections.set(as_i64(n));
        }
        if let Some(n) = inputs.queue_len {
            self.queue_length.set(as_i64(n));
        }
        let wanted = readings.peak().1 * 100.0 >= f64::from(cfg.scale_percent);
        self.scale_wanted.set(i64::from(wanted));
    }
}

fn level_value(level: LoadLevel) -> i64 {
    match level {
        LoadLevel::Normal => 0,
        LoadLevel::Elevated => 1,
        LoadLevel::High => 2,
    }
}

/// Монитор нагрузки узла (PLT-15). Клоны разделяют уровень и показатели.
#[derive(Debug, Clone)]
pub struct NodeLoad {
    level: Arc<watch::Sender<LoadLevel>>,
    inputs: LoadInputs,
}

impl LoadMonitor for NodeLoad {
    fn subscribe(&self) -> LoadRx {
        self.level.subscribe()
    }
}

impl NodeLoad {
    /// Запускает пересчёт уровня до сигнала `shutdown`. `resources` — снимки SYS-06 ([`tp_sys::watch_resources`]),
    /// `updates` — перечитанная конфигурация (секция `load` применяется на ходу). Требует контекста tokio-рантайма.
    pub fn start(
        cfg: LoadConfig,
        resources: watch::Receiver<ResourceSnapshot>,
        updates: Option<watch::Receiver<Arc<Config>>>,
        metrics: &Metrics,
        shutdown: CancellationToken,
    ) -> Self {
        let (level, _) = watch::channel(LoadLevel::Normal);
        let this = Self {
            level: Arc::new(level),
            inputs: LoadInputs::default(),
        };
        let task = Evaluator {
            cfg,
            resources: Some(resources),
            inputs: this.inputs.0.subscribe(),
            updates,
            level: this.level.clone(),
            metrics: LoadMetrics::register(metrics),
        };
        tokio::spawn(task.run(shutdown));
        this
    }

    /// Вход показателей владельцев (пул БД, очереди, кэш, задержка записи, соединения).
    pub fn inputs(&self) -> LoadInputs {
        self.inputs.clone()
    }

    /// Монитор для [`RoleCtx::load`] и [`crate::Roles::with_load`].
    pub fn shared(&self) -> Arc<dyn LoadMonitor> {
        Arc::new(self.clone())
    }
}

/// Задача пересчёта уровня.
struct Evaluator {
    cfg: LoadConfig,
    resources: Option<watch::Receiver<ResourceSnapshot>>,
    inputs: watch::Receiver<Inputs>,
    updates: Option<watch::Receiver<Arc<Config>>>,
    level: Arc<watch::Sender<LoadLevel>>,
    metrics: LoadMetrics,
}

impl Evaluator {
    async fn run(mut self, shutdown: CancellationToken) {
        let mut snap = self.resources.as_mut().map(|rx| *rx.borrow_and_update());
        loop {
            let inputs = *self.inputs.borrow_and_update();
            self.step(snap.as_ref(), &inputs);
            tokio::select! {
                () = shutdown.cancelled() => return,
                new = changed(&mut self.resources) => snap = Some(new),
                r = self.inputs.changed() => {
                    if r.is_err() {
                        return;
                    }
                }
                new = config_changed(&mut self.updates) => {
                    if let Some(new) = new {
                        self.cfg = (*new).clone();
                    }
                }
            }
        }
    }

    fn step(&self, snap: Option<&ResourceSnapshot>, inputs: &Inputs) {
        let readings = match snap {
            Some(snap) => Readings::evaluate(&self.cfg, snap, inputs),
            None => Readings::evaluate(&self.cfg, &unknown_snapshot(), inputs),
        };
        let current = *self.level.borrow();
        let (indicator, peak) = readings.peak();
        let next = self.cfg.next_level(current, peak);
        if next != current {
            tracing::warn!(
                from = %current,
                to = %next,
                indicator = %indicator,
                percent = (peak * 100.0).round() as u64,
                "уровень нагрузки узла изменился"
            );
            self.metrics
                .transitions
                .get_or_create(&vec![("to".to_owned(), next.as_str().to_owned())])
                .inc();
            self.level.send_replace(next);
        }
        self.metrics.observe(&self.cfg, next, &readings, inputs);
    }
}

/// Снимок без сведений о ресурсах: CPU и память не учитываются.
fn unknown_snapshot() -> ResourceSnapshot {
    ResourceSnapshot {
        resources: tp_sys::NodeResources {
            cpus: 1.0,
            memory_total: u64::MAX,
            memory_available: u64::MAX,
        },
        memory_current: None,
        cpu_usage_usec: None,
        cpu_used: None,
    }
}

/// Новый снимок; закрытый канал — ждёт вечно (последний снимок остаётся в силе).
async fn changed(rx: &mut Option<watch::Receiver<ResourceSnapshot>>) -> ResourceSnapshot {
    let Some(r) = rx else {
        return std::future::pending().await;
    };
    if r.changed().await.is_err() {
        *rx = None;
        return std::future::pending().await;
    }
    *r.borrow_and_update()
}

/// Секция `load` после перечитывания конфигурации; без источника обновлений ждёт вечно.
async fn config_changed(
    updates: &mut Option<watch::Receiver<Arc<Config>>>,
) -> Option<Arc<LoadConfig>> {
    let Some(rx) = updates else {
        return std::future::pending().await;
    };
    if rx.changed().await.is_err() {
        *updates = None;
        return None;
    }
    let section = rx.borrow_and_update().section::<LoadConfig>(LOAD);
    section
        .inspect_err(|e| tracing::warn!(error = %e, "load section unavailable"))
        .ok()
}

/// Ждёт, пока фоновые задания разрешены (уровень ниже [`LoadLevel::Elevated`]); `false` — пришёл сигнал остановки.
/// Фоновые роли (индексация, GC, миграция) вызывают перед очередной порцией работы.
pub async fn background_allowed(ctx: &RoleCtx) -> bool {
    let mut rx = ctx.load.subscribe();
    loop {
        if !rx.borrow_and_update().pauses_background() {
            return true;
        }
        tokio::select! {
            () = ctx.shutdown.cancelled() => return false,
            r = rx.changed() => {
                if r.is_err() {
                    // Уровень больше не меняется: ждать остановки.
                    ctx.shutdown.cancelled().await;
                    return false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tp_contracts::role::{ReadyHandle, fixed_config};
    use tp_sys::NodeResources;
    use tp_testkit::MemLoadMonitor;

    use super::*;
    use crate::config::SectionRegistry;

    const GIB: u64 = 1 << 30;

    fn snap(cpus: f64, cpu_used: Option<f64>, total: u64, available: u64) -> ResourceSnapshot {
        ResourceSnapshot {
            resources: NodeResources {
                cpus,
                memory_total: total,
                memory_available: available,
            },
            memory_current: Some(total - available),
            cpu_usage_usec: None,
            cpu_used,
        }
    }

    #[test]
    fn levels_with_hysteresis() {
        let cfg = LoadConfig::default();
        use LoadLevel::*;
        // Подъём — по порогам 85/95.
        assert_eq!(cfg.next_level(Normal, 0.84), Normal);
        assert_eq!(cfg.next_level(Normal, 0.85), Elevated);
        assert_eq!(cfg.next_level(Normal, 0.97), High);
        assert_eq!(cfg.next_level(Elevated, 0.95), High);
        // Снижение — ниже порога на 5 п.п.
        assert_eq!(cfg.next_level(High, 0.91), High);
        assert_eq!(cfg.next_level(High, 0.89), Elevated);
        assert_eq!(cfg.next_level(High, 0.79), Normal);
        assert_eq!(cfg.next_level(Elevated, 0.81), Elevated);
        assert_eq!(cfg.next_level(Elevated, 0.79), Normal);
        // Нормальный уровень не «помнит» гистерезис.
        assert_eq!(cfg.next_level(Normal, 0.82), Normal);
        let none = LoadConfig {
            hysteresis_percent: 0,
            ..LoadConfig::default()
        };
        assert_eq!(none.next_level(High, 0.94), Elevated);
    }

    #[test]
    fn section_parses_and_checks() {
        let cfg: LoadConfig =
            serde_yaml_ng::from_str("elevated_percent: 80\nwrite_latency_max: 500ms\n")
                .expect("parse");
        assert_eq!(cfg.elevated_percent, 80);
        assert_eq!(cfg.high_percent, 95);
        assert_eq!(cfg.write_latency_max, Duration::from_millis(500));
        assert!(cfg.check(Profile::Prod).is_ok());
        assert!(LoadConfig::default().check(Profile::Prod).is_ok());
        for bad in [
            LoadConfig {
                elevated_percent: 95,
                ..LoadConfig::default()
            },
            LoadConfig {
                high_percent: 101,
                ..LoadConfig::default()
            },
            LoadConfig {
                scale_percent: 90,
                ..LoadConfig::default()
            },
        ] {
            assert!(bad.check(Profile::Prod).is_err(), "{bad:?}");
        }
        assert!(serde_yaml_ng::from_str::<LoadConfig>("bogus: 1").is_err());

        let registry = SectionRegistry::new();
        let base = "database: { url: pg }\nblob: { url: file:///b }\ndomain: d\n";
        let c = registry.parse(base, Vec::new()).expect("defaults");
        assert_eq!(
            *c.section::<LoadConfig>(LOAD).expect("load"),
            LoadConfig::default()
        );
        let bad = format!("{base}load: {{ elevated_percent: 99 }}\n");
        assert!(registry.parse(&bad, Vec::new()).is_err());
    }

    #[test]
    fn readings_are_relative_to_current_limits() {
        let cfg = LoadConfig::default();
        let inputs = Inputs {
            cache_disk: Some((25, 100)),
            db_pool: Some((3, 4)),
            queue_len: Some(5_000),
            write_latency: Some(Duration::from_millis(250)),
            connections: Some(10),
        };
        let r = Readings::evaluate(&cfg, &snap(2.0, Some(1.0), 4 * GIB, GIB), &inputs);
        let get = |i| r.0[&i];
        assert!((get(Indicator::Cpu) - 0.5).abs() < 1e-9);
        assert!((get(Indicator::Memory) - 0.75).abs() < 1e-9);
        assert!((get(Indicator::CacheDisk) - 0.75).abs() < 1e-9);
        assert!((get(Indicator::DbPool) - 0.75).abs() < 1e-9);
        assert!((get(Indicator::Queues) - 0.5).abs() < 1e-9);
        assert!((get(Indicator::WriteLatency) - 0.25).abs() < 1e-9);
        // Тот же расход при вдвое большей квоте CPU — вдвое меньшая доля.
        let r = Readings::evaluate(&cfg, &snap(4.0, Some(1.0), 4 * GIB, GIB), &inputs);
        assert!((r.0[&Indicator::Cpu] - 0.25).abs() < 1e-9);

        // Без лимита памяти и замера CPU, без источников и с нулевыми «ёмкостями» — показателей нет.
        let off = LoadConfig {
            queue_capacity: 0,
            write_latency_max: Duration::ZERO,
            ..LoadConfig::default()
        };
        let r = Readings::evaluate(
            &off,
            &snap(1.0, None, u64::MAX, u64::MAX),
            &Inputs {
                queue_len: Some(1),
                write_latency: Some(Duration::from_secs(1)),
                ..Inputs::default()
            },
        );
        assert!(r.0.is_empty(), "{r:?}");
        assert_eq!(r.peak(), (Indicator::Cpu, 0.0));
    }

    async fn wait_level(rx: &mut LoadRx, level: LoadLevel) {
        tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|l| *l == level))
            .await
            .expect("level in time")
            .expect("channel open");
    }

    #[tokio::test]
    async fn monitor_follows_inputs_and_limits() {
        let (res_tx, res_rx) = watch::channel(snap(2.0, Some(0.2), 4 * GIB, 3 * GIB));
        let metrics = Metrics::default();
        let shutdown = CancellationToken::new();
        let load = NodeLoad::start(
            LoadConfig::default(),
            res_rx,
            None,
            &metrics,
            shutdown.clone(),
        );
        let mut rx = load.subscribe();
        assert_eq!(*rx.borrow(), LoadLevel::Normal);

        // Пул БД занят на 90 % — повышенный; затем очереди на 100 % — высокий.
        let inputs = load.inputs();
        inputs.set_db_pool(9, 10);
        wait_level(&mut rx, LoadLevel::Elevated).await;
        inputs.set_queue_len(10_000);
        inputs.set_connections(42);
        wait_level(&mut rx, LoadLevel::High).await;
        inputs.set_queue_len(9_200);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(load.level(), LoadLevel::High, "в пределах гистерезиса");
        inputs.set_queue_len(0);
        inputs.set_db_pool(1, 10);
        wait_level(&mut rx, LoadLevel::Normal).await;

        // Память: 3.6 из 4 GiB — 90 %; лимит cgroup подняли до 8 GiB — тот же расход 45 %.
        res_tx.send_replace(snap(2.0, Some(0.2), 4 * GIB, 4 * GIB / 10));
        wait_level(&mut rx, LoadLevel::Elevated).await;
        let used = 4 * GIB - 4 * GIB / 10;
        res_tx.send_replace(snap(2.0, Some(0.2), 8 * GIB, 8 * GIB - used));
        wait_level(&mut rx, LoadLevel::Normal).await;

        let text = metrics.encode().expect("encode");
        for needle in [
            "tp_load_level 0",
            "tp_load_transitions_total{to=\"high\"} 1",
            "tp_load_transitions_total{to=\"elevated\"} 2",
            "tp_load_transitions_total{to=\"normal\"} 2",
            "tp_load_ratio{indicator=\"memory\"}",
            "tp_load_connections 42",
            "tp_load_queue_length 0",
            "tp_load_scale_wanted 0",
        ] {
            assert!(text.contains(needle), "{needle}\n{text}");
        }
        shutdown.cancel();
    }

    #[tokio::test]
    async fn thresholds_follow_config_reload() {
        let registry = SectionRegistry::new();
        let base = "database: { url: pg }\nblob: { url: file:///b }\ndomain: d\n";
        let parse = |extra: &str| {
            Arc::new(
                registry
                    .parse(&format!("{base}{extra}"), Vec::new())
                    .expect("parse"),
            )
        };
        let (cfg_tx, cfg_rx) = watch::channel(parse(""));
        let (_res_tx, res_rx) = watch::channel(snap(1.0, Some(0.8), 4 * GIB, 4 * GIB));
        let shutdown = CancellationToken::new();
        let load = NodeLoad::start(
            LoadConfig::default(),
            res_rx,
            Some(cfg_rx),
            &Metrics::default(),
            shutdown.clone(),
        );
        let mut rx = load.subscribe();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(*rx.borrow(), LoadLevel::Normal, "CPU 80 % < 85 %");
        cfg_tx.send_replace(parse(
            "load: { scale_percent: 50, elevated_percent: 75, high_percent: 90 }\n",
        ));
        wait_level(&mut rx, LoadLevel::Elevated).await;
        shutdown.cancel();
    }

    fn role_ctx(load: Arc<dyn LoadMonitor>, shutdown: CancellationToken) -> RoleCtx {
        RoleCtx {
            config: fixed_config(()),
            shutdown,
            drain_deadline: Duration::from_secs(1),
            ready: ReadyHandle::default(),
            load,
        }
    }

    #[tokio::test]
    async fn background_work_pauses_on_elevated() {
        let mem = MemLoadMonitor::new(LoadLevel::Elevated);
        let shutdown = CancellationToken::new();
        let ctx = role_ctx(mem.shared(), shutdown.clone());
        let gate = tokio::spawn(async move { background_allowed(&ctx).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !gate.is_finished(),
            "на повышенном уровне фоновые задания ждут"
        );
        mem.set(LoadLevel::Normal);
        assert!(gate.await.expect("join"));

        let mem = MemLoadMonitor::new(LoadLevel::High);
        let ctx = role_ctx(mem.shared(), shutdown.clone());
        let gate = tokio::spawn(async move { background_allowed(&ctx).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        shutdown.cancel();
        assert!(!gate.await.expect("join"), "остановка во время паузы");
    }
}
