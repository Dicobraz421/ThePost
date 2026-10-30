//! Ресурсы узла для автонастройки лимитов (SRS 2.7, 5.5): доступные CPU и память с учётом
//! cgroup v2 (`cpu.max`, `memory.max` по всей цепочке предков), `sched_getaffinity` и `/proc/meminfo`.
//!
//! Чтение синхронное (procfs/sysfs) — вызывать при старте или через `spawn_blocking`.

use std::fs;
use std::path::{Path, PathBuf};

use nix::sched::{CpuSet, sched_getaffinity};
use nix::unistd::Pid;

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
        let Some(own) = read(root, "proc/self/cgroup").and_then(|s| parse_self_cgroup(&s)) else {
            return res;
        };
        // Лимит может быть задан на любом предке: берём минимум по цепочке до корня cgroup.
        let mut dir = cgroup_root.join(own);
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

    #[test]
    fn detect_on_host_is_sane() {
        let r = NodeResources::detect();
        assert!(r.cpus > 0.0);
        assert!(r.memory_total > 0);
        assert!(r.memory_available <= r.memory_total);
    }
}
