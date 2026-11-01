//! Ресурсы узла для автонастройки лимитов (SRS 2.7, 5.5): доступные CPU и память с учётом
//! cgroup v2 (`cpu.max`, `memory.max` по всей цепочке предков), `sched_getaffinity` и `/proc/meminfo`.
//!
//! [`NodeResources::detect`] читает синхронно (procfs/sysfs) — при старте или через `spawn_blocking`.
//! [`watch_resources`] перечитывает лимиты и использование (`memory.current`, `cpu.stat`) с интервалом
//! из [`ResourcesConfig`] и рассылает снимки через `tokio::sync::watch`: изменение лимитов cgroups
//! (VPA, горячее добавление памяти и CPU) доходит до подписчиков без перезапуска (SRS 2.7, 6.3).
//! Опрос, а не inotify: файлы cgroup v2 `memory.max`, `cpu.max` не порождают событий inotify.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nix::sched::{CpuSet, sched_getaffinity};
use nix::unistd::Pid;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};

/// Доступные процессу ресурсы узла.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NodeResources {
    /// Доступные CPU (дробные при квоте cgroup, например 1.5).
    pub cpus: f64,
    /// Предел памяти процесса, байт: минимум из `MemTotal` и `memory.max` cgroup.
    pub memory_total: u64,
    /// Доступная сейчас память, байт: минимум из `MemAvailable` и остатка до `memory.max`.
    pub memory_available: u64,
}

impl NodeResources {
    /// Ресурсы текущего процесса. Не завершается ошибкой: недоступные источники пропускаются.
    pub fn detect() -> Self {
        Self::detect_in(Path::new("/"), affinity_cpus())
    }

    /// Ресурсы по дереву файлов с корнем `root` (`proc/…`, `sys/fs/cgroup/…`) и числу CPU в маске
    /// привязки `affinity`. Используется тестами на фикстурах.
    pub fn detect_in(root: &Path, affinity: usize) -> Self {
        let affinity = affinity.max(1);
        let (mem_total, mem_available) = read(root, "proc/meminfo")
            .map(|s| parse_meminfo(&s))
            .unwrap_or((None, None));
        let mut res = Self {
            cpus: affinity as f64,
            memory_total: mem_total.unwrap_or(u64::MAX),
            memory_available: mem_available.or(mem_total).unwrap_or(u64::MAX),
        };

        let cgroup_root = root.join("sys/fs/cgroup");
        let Some(mut dir) = own_cgroup(root) else {
            return res;
        };
        // Лимит может быть задан на любом предке: берём минимум по цепочке до корня cgroup.
        loop {
            if let Some(cpus) = read_file(&dir.join("cpu.max")).and_then(|s| parse_cpu_max(&s)) {
                res.cpus = res.cpus.min(cpus);
            }
            if let Some(max) = read_file(&dir.join("memory.max")).and_then(|s| parse_memory_max(&s))
            {
                res.memory_total = res.memory_total.min(max);
                let current = read_file(&dir.join("memory.current"))
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or(0);
                res.memory_available = res.memory_available.min(max.saturating_sub(current));
            }
            if dir == cgroup_root || !dir.pop() || !dir.starts_with(&cgroup_root) {
                break;
            }
        }
        res.memory_available = res.memory_available.min(res.memory_total);
        res
    }

    /// Число CPU для размера пулов: округление вверх, не меньше 1.
    pub fn cpus_ceil(&self) -> usize {
        (self.cpus.ceil() as usize).max(1)
    }
}

/// Секция `resources`: перечитывание ресурсов узла (SRS 2.7, 5.5).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ResourcesConfig {
    /// Интервал перечитывания лимитов и использования cgroups, мс; 0 — только чтение при старте.
    pub poll_interval_ms: u64,
}

impl Default for ResourcesConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 5_000,
        }
    }
}

/// Лимиты и использование ресурсов узла на момент чтения.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceSnapshot {
    /// Лимиты CPU и памяти, доступная память.
    pub resources: NodeResources,
    /// Память cgroup процесса (`memory.current`), байт; `None` — вне cgroup v2 или нет контроллера.
    pub memory_current: Option<u64>,
    /// Время CPU cgroup процесса нарастающим итогом (`cpu.stat`, `usage_usec`), мкс.
    pub cpu_usage_usec: Option<u64>,
    /// Средняя загрузка CPU за последний интервал (1.0 — одно ядро); `None` до второго чтения.
    pub cpu_used: Option<f64>,
}

impl ResourceSnapshot {
    /// Снимок текущего процесса (синхронное чтение procfs/sysfs).
    pub fn detect() -> Self {
        Self::detect_in(Path::new("/"), affinity_cpus())
    }

    /// Снимок по дереву файлов с корнем `root` (как [`NodeResources::detect_in`]).
    pub fn detect_in(root: &Path, affinity: usize) -> Self {
        let own = own_cgroup(root);
        let leaf = |name: &str| own.as_ref().and_then(|dir| read_file(&dir.join(name)));
        Self {
            resources: NodeResources::detect_in(root, affinity),
            memory_current: leaf("memory.current").and_then(|s| s.trim().parse().ok()),
            cpu_usage_usec: leaf("cpu.stat").and_then(|s| parse_cpu_stat_usage(&s)),
            cpu_used: None,
        }
    }

    /// Снимок без сведений: 1 CPU, память без предела (если чтение не выполнилось).
    fn unknown() -> Self {
        Self {
            resources: NodeResources {
                cpus: 1.0,
                memory_total: u64::MAX,
                memory_available: u64::MAX,
            },
            memory_current: None,
            cpu_usage_usec: None,
            cpu_used: None,
        }
    }
}

/// Подписка на ресурсы текущего процесса: первый снимок — сразу, далее — с интервалом из `cfg`.
/// Фоновая задача завершается, когда закрыты все получатели. Вызывать внутри рантайма tokio.
pub async fn watch_resources(cfg: &ResourcesConfig) -> watch::Receiver<ResourceSnapshot> {
    watch_resources_in(PathBuf::from("/"), None, cfg).await
}

/// Подписка по дереву файлов с корнем `root`; `affinity` — фиксированное число CPU в маске
/// привязки (тесты), `None` — `sched_getaffinity` при каждом чтении.
pub async fn watch_resources_in(
    root: PathBuf,
    affinity: Option<usize>,
    cfg: &ResourcesConfig,
) -> watch::Receiver<ResourceSnapshot> {
    let root: Arc<Path> = root.into();
    let first = probe(&root, affinity)
        .await
        .unwrap_or_else(ResourceSnapshot::unknown);
    let (tx, rx) = watch::channel(first);
    if cfg.poll_interval_ms == 0 {
        return rx;
    }
    let period = Duration::from_millis(cfg.poll_interval_ms);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tick.tick().await;
        let mut prev = (Instant::now(), first.cpu_usage_usec);
        loop {
            tokio::select! {
                () = tx.closed() => break,
                _ = tick.tick() => {}
            }
            let Some(mut snap) = probe(&root, affinity).await else {
                continue;
            };
            let now = Instant::now();
            snap.cpu_used = cpu_rate(prev, (now, snap.cpu_usage_usec));
            prev = (now, snap.cpu_usage_usec);
            let old = tx.borrow().resources;
            if old != snap.resources {
                tracing::info!(
                    cpus = snap.resources.cpus,
                    memory_total = snap.resources.memory_total,
                    "лимиты ресурсов узла изменились"
                );
            }
            tx.send_replace(snap);
        }
    });
    rx
}

/// Чтение снимка в пуле блокирующих задач; `None` — задача чтения не выполнилась.
async fn probe(root: &Arc<Path>, affinity: Option<usize>) -> Option<ResourceSnapshot> {
    let root = Arc::clone(root);
    tokio::task::spawn_blocking(move || {
        ResourceSnapshot::detect_in(&root, affinity.unwrap_or_else(affinity_cpus))
    })
    .await
    .ok()
}

/// Средняя загрузка CPU между двумя чтениями `cpu.stat`.
fn cpu_rate(prev: (Instant, Option<u64>), cur: (Instant, Option<u64>)) -> Option<f64> {
    let used = cur.1?.checked_sub(prev.1?)?;
    let wall = cur.0.checked_duration_since(prev.0)?.as_micros();
    (wall > 0).then(|| used as f64 / wall as f64)
}

/// Число CPU в маске привязки процесса (`sched_getaffinity`); при ошибке — 1.
pub fn affinity_cpus() -> usize {
    match sched_getaffinity(Pid::from_raw(0)) {
        Ok(set) => (0..CpuSet::count())
            .filter(|&i| set.is_set(i).unwrap_or(false))
            .count()
            .max(1),
        Err(_) => 1,
    }
}

fn read(root: &Path, rel: &str) -> Option<String> {
    read_file(&root.join(rel))
}

fn read_file(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Каталог cgroup v2 процесса в дереве `root`.
fn own_cgroup(root: &Path) -> Option<PathBuf> {
    let own = read(root, "proc/self/cgroup").and_then(|s| parse_self_cgroup(&s))?;
    Some(root.join("sys/fs/cgroup").join(own))
}

/// `usage_usec` из `cpu.stat`.
fn parse_cpu_stat_usage(s: &str) -> Option<u64> {
    s.lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
}

/// Путь cgroup v2 из `/proc/self/cgroup` (строка `0::/путь`) относительно корня cgroup.
fn parse_self_cgroup(s: &str) -> Option<PathBuf> {
    s.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| PathBuf::from(p.trim().trim_start_matches('/')))
}

/// `cpu.max`: `max 100000` — без ограничения, `150000 100000` — 1.5 CPU.
fn parse_cpu_max(s: &str) -> Option<f64> {
    let mut it = s.split_whitespace();
    let quota = it.next()?;
    if quota == "max" {
        return None;
    }
    let quota: f64 = quota.parse().ok()?;
    let period: f64 = it.next().map_or(Some(100_000.0), |p| p.parse().ok())?;
    (quota > 0.0 && period > 0.0).then(|| quota / period)
}

/// `memory.max`: `max` — без ограничения, иначе байты.
fn parse_memory_max(s: &str) -> Option<u64> {
    let s = s.trim();
    if s == "max" { None } else { s.parse().ok() }
}

/// `MemTotal` и `MemAvailable` из `/proc/meminfo` (значения в kB), байт.
fn parse_meminfo(s: &str) -> (Option<u64>, Option<u64>) {
    let field = |name: &str| {
        s.lines().find_map(|l| {
            let rest = l.strip_prefix(name)?.strip_prefix(':')?;
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            Some(kb.saturating_mul(1024))
        })
    };
    (field("MemTotal"), field("MemAvailable"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;
    const MEMINFO: &str = "MemTotal:       16777216 kB\nMemFree:         1048576 kB\n\
                           MemAvailable:    8388608 kB\nBuffers:           10240 kB\n";

    /// Временное дерево `proc/…` и `sys/fs/cgroup/…`.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str, files: &[(&str, &str)]) -> Self {
            let root = std::env::temp_dir().join(format!("tp-sys-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            for (rel, content) in files {
                let path = root.join(rel);
                fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
                fs::write(path, content).expect("write");
            }
            Self(root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parsers() {
        assert_eq!(parse_cpu_max("max 100000\n"), None);
        assert_eq!(parse_cpu_max("150000 100000\n"), Some(1.5));
        assert_eq!(parse_cpu_max("50000\n"), Some(0.5));
        assert_eq!(parse_memory_max("max\n"), None);
        assert_eq!(parse_memory_max("536870912\n"), Some(512 << 20));
        assert_eq!(parse_meminfo(MEMINFO), (Some(16 * GIB), Some(8 * GIB)));
        assert_eq!(
            parse_self_cgroup("0::/system.slice/thepost.service\n"),
            Some(PathBuf::from("system.slice/thepost.service"))
        );
        assert_eq!(parse_self_cgroup("12:cpu,cpuacct:/x\n"), None);
        assert_eq!(
            parse_cpu_stat_usage("usage_usec 1500000\nuser_usec 1000000\n"),
            Some(1_500_000)
        );
        assert_eq!(parse_cpu_stat_usage("nr_periods 0\n"), None);
    }

    #[test]
    fn no_cgroup_uses_affinity_and_meminfo() {
        let f = Fixture::new("plain", &[("proc/meminfo", MEMINFO)]);
        let r = NodeResources::detect_in(&f.0, 8);
        assert_eq!(r.cpus, 8.0);
        assert_eq!(r.memory_total, 16 * GIB);
        assert_eq!(r.memory_available, 8 * GIB);
    }

    #[test]
    fn unlimited_cgroup() {
        let f = Fixture::new(
            "unlimited",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "0::/\n"),
                ("sys/fs/cgroup/cpu.max", "max 100000\n"),
                ("sys/fs/cgroup/memory.max", "max\n"),
            ],
        );
        let r = NodeResources::detect_in(&f.0, 4);
        assert_eq!(
            (r.cpus, r.memory_total, r.memory_available),
            (4.0, 16 * GIB, 8 * GIB)
        );
    }

    #[test]
    fn container_limits() {
        let f = Fixture::new(
            "limited",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "0::/kubepods/pod1/c1\n"),
                ("sys/fs/cgroup/kubepods/pod1/c1/cpu.max", "150000 100000\n"),
                ("sys/fs/cgroup/kubepods/pod1/c1/memory.max", "2147483648\n"),
                (
                    "sys/fs/cgroup/kubepods/pod1/c1/memory.current",
                    "536870912\n",
                ),
            ],
        );
        let r = NodeResources::detect_in(&f.0, 16);
        assert_eq!(r.cpus, 1.5);
        assert_eq!(r.cpus_ceil(), 2);
        assert_eq!(r.memory_total, 2 * GIB);
        assert_eq!(r.memory_available, 2 * GIB - (512 << 20));
    }

    #[test]
    fn ancestor_limit_is_minimum() {
        let f = Fixture::new(
            "nested",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "0::/system.slice/thepost.service\n"),
                ("sys/fs/cgroup/system.slice/cpu.max", "200000 100000\n"),
                ("sys/fs/cgroup/system.slice/memory.max", "1073741824\n"),
                (
                    "sys/fs/cgroup/system.slice/thepost.service/cpu.max",
                    "max 100000\n",
                ),
                (
                    "sys/fs/cgroup/system.slice/thepost.service/memory.max",
                    "4294967296\n",
                ),
            ],
        );
        let r = NodeResources::detect_in(&f.0, 1);
        assert_eq!(r.cpus, 1.0, "affinity ниже квоты");
        assert_eq!(r.memory_total, GIB);
        assert_eq!(r.memory_available, GIB);
    }

    /// Изменение `memory.max` в подменном каталоге cgroups доходит до подписчика (SYS-06).
    #[tokio::test]
    async fn memory_max_change_reaches_subscriber() {
        let f = Fixture::new(
            "watch",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "0::/pod1/c1\n"),
                ("sys/fs/cgroup/pod1/c1/cpu.max", "200000 100000\n"),
                ("sys/fs/cgroup/pod1/c1/memory.max", "2147483648\n"),
                ("sys/fs/cgroup/pod1/c1/memory.current", "536870912\n"),
                ("sys/fs/cgroup/pod1/c1/cpu.stat", "usage_usec 1000\n"),
            ],
        );
        let cfg = ResourcesConfig {
            poll_interval_ms: 10,
        };
        let mut rx = watch_resources_in(f.0.clone(), Some(8), &cfg).await;
        {
            let first = rx.borrow_and_update();
            assert_eq!(first.resources.cpus, 2.0);
            assert_eq!(first.resources.memory_total, 2 * GIB);
            assert_eq!(first.memory_current, Some(512 << 20));
            assert_eq!(first.cpu_usage_usec, Some(1000));
            assert_eq!(first.cpu_used, None);
        }

        let c1 = f.0.join("sys/fs/cgroup/pod1/c1");
        fs::write(c1.join("memory.max"), "1073741824\n").expect("write");
        fs::write(c1.join("cpu.max"), "100000 100000\n").expect("write");
        let snap = *tokio::time::timeout(
            Duration::from_secs(10),
            rx.wait_for(|s| s.resources.memory_total == GIB && s.resources.cpus == 1.0),
        )
        .await
        .expect("изменение не дошло за 10 с")
        .expect("канал закрыт");
        assert_eq!(snap.resources.memory_available, GIB - (512 << 20));
        assert_eq!(snap.memory_current, Some(512 << 20));
    }

    #[tokio::test]
    async fn zero_interval_reads_once() {
        let f = Fixture::new("watch-once", &[("proc/meminfo", MEMINFO)]);
        let cfg = ResourcesConfig {
            poll_interval_ms: 0,
        };
        let mut rx = watch_resources_in(f.0.clone(), Some(2), &cfg).await;
        assert_eq!(rx.borrow().resources.memory_total, 16 * GIB);
        assert_eq!(rx.borrow().memory_current, None);
        assert!(
            rx.changed().await.is_err(),
            "без перечитывания канал закрыт"
        );
    }

    #[test]
    fn cpu_rate_between_reads() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(cpu_rate((t0, Some(0)), (t1, Some(3_000_000))), Some(1.5));
        assert_eq!(cpu_rate((t0, None), (t1, Some(1))), None);
        assert_eq!(cpu_rate((t0, Some(5)), (t1, Some(1))), None);
    }

    #[test]
    fn config_defaults() {
        let cfg: ResourcesConfig = serde_yaml_ng::from_str("{}").expect("yaml");
        assert_eq!(cfg, ResourcesConfig::default());
        assert!(serde_yaml_ng::from_str::<ResourcesConfig>("poll: 1").is_err());
    }

    #[test]
    fn detect_on_host_is_sane() {
        let r = NodeResources::detect();
        assert!(r.cpus > 0.0);
        assert!(r.memory_total > 0);
        assert!(r.memory_available <= r.memory_total);
    }
}
