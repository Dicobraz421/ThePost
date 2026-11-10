//! Давление ресурсов узла (SRS 2.8 — закрытый список таймеров п. 5, SRS 6.3): снимки ресурсов
//! ([`ResourceSnapshot`]) перечитываются по триггерам PSI cgroups, а не по интервалу.
//!
//! Триггер — строка `some <задержка> <окно>` в файле `<ресурс>.pressure` cgroup процесса (cgroup v2), при его
//! отсутствии — в `/proc/pressure/<ресурс>`; ядро сообщает о превышении порога событием `POLLPRI` не чаще раза за
//! окно. По событию снимок перечитывается сразу, ещё раз — после окна без событий (давление спало), дальше —
//! ожидание следующего события, без таймера.
//!
//! Интервальное перечитывание SYS-06 ([`tp_sys::watch_resources`], секция `resources`) — только если ни один
//! триггер не поставлен (нет PSI, нет прав) или события прекратились.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tp_contracts::Error;
use tp_contracts::config::duration;
use tp_contracts::tls::Profile;
use tp_sys::{ResourceSnapshot, ResourcesConfig};

/// Имя секции.
pub const PRESSURE: &str = "pressure";

/// Секция `pressure`: триггеры PSI (Pressure Stall Information). Требует перезапуска.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PressureConfig {
    /// Ресурсы с триггерами; пусто — триггеры не ставятся, ресурсы перечитываются с интервалом секции `resources`.
    pub resources: Vec<PressureResource>,
    /// Порог: суммарная задержка задач из-за нехватки ресурса (`some`) за окно (`150ms`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub stall: Duration,
    /// Окно триггера (`2s`): от 500ms до 10s; без CAP_SYS_RESOURCE ядро принимает только окно, кратное 2s.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub window: Duration,
}

impl Default for PressureConfig {
    fn default() -> Self {
        Self {
            resources: vec![
                PressureResource::Memory,
                PressureResource::Cpu,
                PressureResource::Io,
            ],
            stall: Duration::from_millis(150),
            window: Duration::from_secs(2),
        }
    }
}

impl PressureConfig {
    /// Проверка секции: окно 500ms–10s, `0 < stall < window`.
    pub fn check(&self, _profile: Profile) -> tp_contracts::Result<()> {
        let window_ok =
            self.window >= Duration::from_millis(500) && self.window <= Duration::from_secs(10);
        if !window_ok || self.stall.is_zero() || self.stall >= self.window {
            return Err(Error::Invalid(format!(
                "{PRESSURE}: required 0 < stall < window, window from 500ms to 10s, got {:?} / {:?}",
                self.stall, self.window
            )));
        }
        Ok(())
    }

    /// Строка триггера для файла PSI.
    fn trigger(&self) -> String {
        format!(
            "some {} {}",
            self.stall.as_micros(),
            self.window.as_micros()
        )
    }
}

/// Ресурс PSI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PressureResource {
    /// Память.
    Memory,
    /// CPU.
    Cpu,
    /// Ввод-вывод.
    Io,
}

impl PressureResource {
    /// Имя ресурса в файлах PSI.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Cpu => "cpu",
            Self::Io => "io",
        }
    }
}

/// Откуда приходят новые снимки ресурсов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceSource {
    /// По событиям триггеров PSI.
    Pressure,
    /// По интервалу секции `resources` (PSI недоступен).
    Poll,
}

/// Подписка на снимки ресурсов узла.
#[derive(Debug, Clone)]
pub struct ResourceWatch {
    rx: watch::Receiver<ResourceSnapshot>,
    source: ResourceSource,
}

impl ResourceWatch {
    /// Получатель снимков (для [`crate::NodeLoad::start`]).
    pub fn subscribe(&self) -> watch::Receiver<ResourceSnapshot> {
        self.rx.clone()
    }

    /// Источник снимков на момент запуска.
    pub fn source(&self) -> ResourceSource {
        self.source
    }
}

/// Снимки ресурсов текущего процесса: по триггерам PSI, без них — с интервалом `resources`.
/// Первый снимок — сразу. Фоновые задачи завершаются, когда закрыты все получатели. Требует рантайма tokio.
pub async fn watch_resources(
    resources: &ResourcesConfig,
    pressure: &PressureConfig,
) -> ResourceWatch {
    watch_resources_in(PathBuf::from("/"), None, resources, pressure).await
}

/// Как [`watch_resources`] по дереву файлов с корнем `root`; `affinity` — фиксированное число CPU (тесты).
pub async fn watch_resources_in(
    root: PathBuf,
    affinity: Option<usize>,
    resources: &ResourcesConfig,
    pressure: &PressureConfig,
) -> ResourceWatch {
    let (events_tx, events) = mpsc::channel(1);
    let installed = install(&root, pressure, &events_tx).await;
    drop(events_tx);
    if installed == 0 {
        if !pressure.resources.is_empty() {
            tracing::info!(
                interval_ms = resources.poll_interval_ms,
                "PSI triggers unavailable; node resources are re-read by interval"
            );
        }
        return ResourceWatch {
            rx: tp_sys::watch_resources_in(root, affinity, resources).await,
            source: ResourceSource::Poll,
        };
    }
    let fallback = Duration::from_millis(resources.poll_interval_ms);
    ResourceWatch {
        rx: follow(root.into(), affinity, events, pressure.window, fallback).await,
        source: ResourceSource::Pressure,
    }
}

/// Ставит триггеры ресурсов секции; события — в `tx`. Число поставленных триггеров.
async fn install(root: &Path, cfg: &PressureConfig, tx: &mpsc::Sender<()>) -> usize {
    if cfg.resources.is_empty() {
        return 0;
    }
    let (root_owned, cfg_owned) = (root.to_owned(), cfg.clone());
    let opened = tokio::task::spawn_blocking(move || open_triggers(&root_owned, &cfg_owned))
        .await
        .unwrap_or_default();
    let mut installed = 0;
    for (resource, candidates) in opened {
        // Первый файл, который ядро принимает для ожидания событий; остальные закрываются (триггер снимается).
        let fd = candidates.into_iter().find_map(|(path, file)| {
            // Замена `register_with_interest` требует `unsafe` (вне tp-sys запрещён). Условие её безопасности
            // выполнено: дескриптор принадлежит `File`, который живёт внутри `AsyncFd` до его удаления.
            // Перенос ожидания PSI в tp-sys — вопрос владельцу (PLT-17).
            #[allow(deprecated)]
            let registered = AsyncFd::with_interest(file, Interest::PRIORITY);
            match registered {
                Ok(fd) => {
                    tracing::debug!(resource = resource.as_str(), path = %path.display(), "PSI trigger installed");
                    Some(fd)
                }
                Err(e) => {
                    tracing::debug!(path = %path.display(), error = %e, "PSI file cannot be polled");
                    None
                }
            }
        });
        if let Some(fd) = fd {
            spawn_trigger(fd, resource, tx.clone());
            installed += 1;
        }
    }
    installed
}

/// Файлы PSI с записанным триггером по ресурсам: сначала cgroup процесса, затем `/proc/pressure`. Блокирующий I/O.
fn open_triggers(
    root: &Path,
    cfg: &PressureConfig,
) -> Vec<(PressureResource, Vec<(PathBuf, File)>)> {
    let trigger = cfg.trigger();
    let cgroup = std::fs::read_to_string(root.join("proc/self/cgroup"))
        .ok()
        .and_then(|s| own_cgroup(&s))
        .map(|own| root.join("sys/fs/cgroup").join(own));
    cfg.resources
        .iter()
        .map(|&resource| {
            let name = resource.as_str();
            let mut paths = Vec::new();
            if let Some(dir) = &cgroup {
                paths.push(dir.join(format!("{name}.pressure")));
            }
            paths.push(root.join("proc/pressure").join(name));
            let files = paths
                .into_iter()
                .filter_map(|path| match open_trigger(&path, &trigger) {
                    Ok(file) => Some((path, file)),
                    Err(e) => {
                        tracing::debug!(path = %path.display(), error = %e, "PSI trigger not written");
                        None
                    }
                })
                .collect();
            (resource, files)
        })
        .collect()
}

fn open_trigger(path: &Path, trigger: &str) -> std::io::Result<File> {
    let mut file = File::options().read(true).write(true).open(path)?;
    file.write_all(trigger.as_bytes())?;
    Ok(file)
}

/// Путь cgroup v2 процесса из `/proc/self/cgroup` (строка `0::/путь`).
fn own_cgroup(s: &str) -> Option<PathBuf> {
    s.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| PathBuf::from(p.trim().trim_start_matches('/')))
}

/// Ожидание событий одного триггера; завершается с ошибкой файла или когда закрыт получатель событий.
fn spawn_trigger(fd: AsyncFd<File>, resource: PressureResource, tx: mpsc::Sender<()>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = tx.closed() => return,
                ready = fd.ready(Interest::PRIORITY) => match ready {
                    Ok(mut guard) => {
                        if guard.ready().is_error() {
                            tracing::warn!(resource = resource.as_str(), "PSI trigger closed by kernel");
                            return;
                        }
                        guard.clear_ready();
                        // Полный канал — событие уже ждёт обработки: склейка.
                        let _ = tx.try_send(());
                    }
                    Err(e) => {
                        tracing::warn!(resource = resource.as_str(), error = %e, "PSI trigger failed");
                        return;
                    }
                }
            }
        }
    });
}

/// Перечитывание снимков по событиям `events`: сразу и после `quiet` без событий. Когда события прекратились —
/// сверка сразу, дальше с интервалом `fallback` (`0` — больше не перечитывать).
async fn follow(
    root: Arc<Path>,
    affinity: Option<usize>,
    events: mpsc::Receiver<()>,
    quiet: Duration,
    fallback: Duration,
) -> watch::Receiver<ResourceSnapshot> {
    let first = probe(&root, affinity).await.unwrap_or_else(unknown);
    let (tx, rx) = watch::channel(first);
    tokio::spawn(async move {
        let mut events = Some(events);
        let mut calm: Option<Instant> = None;
        let mut poll: Option<Duration> = None;
        let mut prev = (Instant::now(), first.cpu_usage_usec);
        loop {
            tokio::select! {
                () = tx.closed() => return,
                event = recv_or_never(&mut events) => {
                    if event.is_none() {
                        tracing::warn!("PSI triggers stopped; node resources are re-read by interval");
                        events = None;
                        calm = None;
                        poll = (!fallback.is_zero()).then_some(fallback);
                    } else {
                        calm = Some(Instant::now() + quiet);
                    }
                }
                () = sleep_until_or_never(calm) => calm = None,
                () = sleep_or_never(poll) => {}
            }
            let Some(mut snap) = probe(&root, affinity).await else {
                continue;
            };
            let now = Instant::now();
            snap.cpu_used = cpu_rate(prev, (now, snap.cpu_usage_usec));
            prev = (now, snap.cpu_usage_usec);
            tx.send_replace(snap);
        }
    });
    rx
}

async fn recv_or_never(events: &mut Option<mpsc::Receiver<()>>) -> Option<()> {
    match events {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until_or_never(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn sleep_or_never(period: Option<Duration>) {
    match period {
        Some(period) => tokio::time::sleep(period).await,
        None => std::future::pending().await,
    }
}

/// Снимок в пуле блокирующих задач; `None` — задача чтения не выполнилась.
async fn probe(root: &Arc<Path>, affinity: Option<usize>) -> Option<ResourceSnapshot> {
    let root = Arc::clone(root);
    tokio::task::spawn_blocking(move || {
        ResourceSnapshot::detect_in(
            &root,
            affinity.unwrap_or_else(tp_sys::resources::affinity_cpus),
        )
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

/// Снимок без сведений: 1 CPU, память без предела.
fn unknown() -> ResourceSnapshot {
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    struct Root(PathBuf);

    impl Root {
        /// Дерево процесса в cgroup `/thepost` с `memory.current` = `current`.
        fn new(tag: &str, current: u64) -> Self {
            let root = std::env::temp_dir().join(format!(
                "tp-platform-psi-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("proc/self")).expect("proc");
            fs::create_dir_all(root.join("sys/fs/cgroup/thepost")).expect("cgroup");
            fs::write(root.join("proc/self/cgroup"), "0::/thepost\n").expect("cgroup");
            let this = Self(root);
            this.set_current(current);
            this
        }

        fn set_current(&self, current: u64) {
            fs::write(
                self.0.join("sys/fs/cgroup/thepost/memory.current"),
                current.to_string(),
            )
            .expect("memory.current");
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn wait_current(rx: &mut watch::Receiver<ResourceSnapshot>, current: u64) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while rx.borrow_and_update().memory_current != Some(current) {
                rx.changed().await.expect("open");
            }
        })
        .await
        .expect("snapshot in time");
    }

    #[test]
    fn section_parses_and_checks() {
        let cfg = PressureConfig::default();
        assert_eq!(cfg.trigger(), "some 150000 2000000");
        assert!(cfg.check(Profile::Prod).is_ok());
        let parsed: PressureConfig =
            serde_yaml_ng::from_str("resources: [memory]\nstall: 100ms\nwindow: 1s\n")
                .expect("yaml");
        assert_eq!(parsed.resources, vec![PressureResource::Memory]);
        assert_eq!(parsed.trigger(), "some 100000 1000000");
        for bad in [
            "window: 100ms",
            "window: 11s",
            "stall: 0s",
            "stall: 2s\nwindow: 2s",
        ] {
            let cfg: PressureConfig = serde_yaml_ng::from_str(bad).expect("yaml");
            assert!(cfg.check(Profile::Prod).is_err(), "{bad}");
        }
        assert!(serde_yaml_ng::from_str::<PressureConfig>("resources: [disk]").is_err());
        assert_eq!(own_cgroup("1:cpu:/\n0::/a/b\n"), Some(PathBuf::from("a/b")));
    }

    #[tokio::test]
    async fn interval_fallback_without_psi() {
        // Файлы PSI — обычные файлы: триггер записывается, но ожидать событий на них нельзя.
        let root = Root::new("fallback", 100);
        fs::write(root.0.join("sys/fs/cgroup/thepost/memory.pressure"), "").expect("psi");
        fs::create_dir_all(root.0.join("proc/pressure")).expect("proc pressure");
        fs::write(root.0.join("proc/pressure/memory"), "").expect("psi");
        let resources = ResourcesConfig {
            poll_interval_ms: 20,
        };
        let watch = watch_resources_in(
            root.0.clone(),
            Some(1),
            &resources,
            &PressureConfig::default(),
        )
        .await;
        assert_eq!(watch.source(), ResourceSource::Poll);
        let mut rx = watch.subscribe();
        assert_eq!(rx.borrow_and_update().memory_current, Some(100));
        root.set_current(200);
        wait_current(&mut rx, 200).await;

        let off = PressureConfig {
            resources: Vec::new(),
            ..PressureConfig::default()
        };
        let watch = watch_resources_in(root.0.clone(), Some(1), &resources, &off).await;
        assert_eq!(watch.source(), ResourceSource::Poll);
    }

    #[tokio::test]
    async fn event_rereads_at_once_and_after_calm() {
        let root = Root::new("events", 100);
        let (tx, events) = mpsc::channel(1);
        let hour = Duration::from_secs(3600);
        let mut rx = follow(
            root.0.clone().into(),
            Some(1),
            events,
            Duration::from_millis(500),
            hour,
        )
        .await;
        assert_eq!(rx.borrow_and_update().memory_current, Some(100));

        root.set_current(200);
        tx.send(()).await.expect("event");
        wait_current(&mut rx, 200).await;
        // Давление спало: снимок ещё раз после окна без событий.
        root.set_current(150);
        wait_current(&mut rx, 150).await;

        // События прекратились: сверка сразу, дальше — интервал `resources` (здесь час), без новых снимков.
        root.set_current(120);
        drop(tx);
        wait_current(&mut rx, 120).await;
        root.set_current(130);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(rx.borrow_and_update().memory_current, Some(120));
    }

    #[tokio::test]
    async fn psi_triggers_on_this_host() {
        let resources = ResourcesConfig {
            poll_interval_ms: 0,
        };
        let watch = watch_resources(&resources, &PressureConfig::default()).await;
        let psi = Path::new("/proc/pressure/memory").exists();
        // Без PSI или без прав на триггер — запасной интервал; проверка источника только при доступном PSI.
        if !psi || watch.source() == ResourceSource::Poll {
            eprintln!(
                "PSI triggers unavailable here; source = {:?}",
                watch.source()
            );
            return;
        }
        assert_eq!(watch.source(), ResourceSource::Pressure);
    }
}
