//! Hot reload конфигурации (SRS 5.5, CONTRACTS.md#Config): по SIGHUP, по запросу и по опросу mtime
//! файл перечитывается, проверяется и рассылается через `watch`. Ошибка — прежняя версия остаётся в силе.
//!
//! С хранилищем секретов ([`Reloader::with_secret_store`]) каждая версия рассылается с прочитанными секретами
//! (`Config::secret`): секреты читаются при запуске и при каждом перечитывании — по SIGHUP и запросу даже без
//! изменения файла (ротация секрета); аренды динамических секретов продлеваются после двух третей срока, истёкшая
//! или непродлеваемая аренда — секреты перечитываются.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tp_contracts::Error;
use tp_contracts::config::duration;
use tp_contracts::role::{CancellationToken, ConfigRx, ConfigValue};
use tp_contracts::secrets::{SecretLease, SecretStore};

use crate::config::{CheckMode, Config, ConfigError, SectionRegistry};

/// Секция `reload`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ReloadConfig {
    /// Период проверки mtime config.yaml (`5s`); `0s` — только по SIGHUP и запросу. Применяется без перезапуска.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub poll_interval: Duration,
}

impl Default for ReloadConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
        }
    }
}

/// Отметка версии файла для опроса: время изменения и размер.
type Stamp = (Option<SystemTime>, u64);

/// Хранилище секретов перечитывания.
#[derive(Clone)]
struct Store(Arc<dyn SecretStore>);

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretStore")
    }
}

/// Аренда секрета текущей версии и момент следующего действия (продление или перечитывание).
#[derive(Debug, Clone)]
struct Tracked {
    lease: SecretLease,
    due: Instant,
}

/// Перечитывает config.yaml и рассылает новые версии подписчикам.
#[derive(Debug)]
pub struct Reloader {
    registry: Arc<SectionRegistry>,
    path: PathBuf,
    env: Vec<(String, String)>,
    tx: watch::Sender<Arc<Config>>,
    applied: String,
    stamp: Option<Stamp>,
    request: Arc<Notify>,
    secrets: Option<Store>,
    leases: Vec<Tracked>,
}

/// Запрос перечитать конфигурацию (Admin API, тесты) — то же, что SIGHUP.
#[derive(Debug, Clone)]
pub struct ReloadTrigger(Arc<Notify>);

impl ReloadTrigger {
    /// Просит перечитать конфигурацию; повторные запросы до обработки объединяются.
    pub fn request(&self) {
        self.0.notify_one();
    }
}

/// Причина пробуждения цикла [`Reloader::run`].
enum Wake {
    Stop,
    Forced,
    Poll,
    Lease,
}

impl Reloader {
    /// Первая загрузка `path` с переменными окружения процесса (снимок на время работы).
    /// Блокирующий I/O: вызывать при запуске или через `spawn_blocking`.
    pub fn start(registry: Arc<SectionRegistry>, path: PathBuf) -> tp_contracts::Result<Self> {
        Ok(Self::start_with(
            registry,
            path,
            std::env::vars().collect(),
        )?)
    }

    pub(crate) fn start_with(
        registry: Arc<SectionRegistry>,
        path: PathBuf,
        env: Vec<(String, String)>,
    ) -> Result<Self, ConfigError> {
        let applied = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?;
        let stamp = std::fs::metadata(&path).ok().map(|m| stamp_of(&m));
        let config = registry.parse_with(&applied, env.clone(), CheckMode::Lenient)?;
        let (tx, _) = watch::channel(Arc::new(config));
        Ok(Self {
            registry,
            path,
            env,
            tx,
            applied,
            stamp,
            request: Arc::new(Notify::new()),
            secrets: None,
            leases: Vec::new(),
        })
    }

    /// Подключает хранилище секретов: читает секреты текущей версии и рассылает её с ними; дальше секреты
    /// читаются при каждом перечитывании. Ошибка чтения — `Config::resolve_secrets`.
    pub async fn with_secret_store(
        mut self,
        store: Arc<dyn SecretStore>,
    ) -> tp_contracts::Result<Self> {
        let mut config = (*self.current()).clone();
        config.resolve_secrets(store.as_ref()).await?;
        self.secrets = Some(Store(store));
        self.publish(config);
        Ok(self)
    }

    /// Подписка на версии конфигурации.
    pub fn subscribe(&self) -> watch::Receiver<Arc<Config>> {
        self.tx.subscribe()
    }

    /// Текущая версия.
    pub fn current(&self) -> Arc<Config> {
        self.tx.borrow().clone()
    }

    /// Источник запросов перечитать конфигурацию.
    pub fn trigger(&self) -> ReloadTrigger {
        ReloadTrigger(self.request.clone())
    }

    /// Перечитывает файл (и секреты, если подключено хранилище). `Ok(true)` — разослана новая версия,
    /// `Ok(false)` — не изменились ни текст, ни секреты; ошибка — версия не изменилась, подписчики ничего не получают.
    pub async fn reload(&mut self) -> tp_contracts::Result<bool> {
        let text = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(|source| ConfigError::Io {
                path: self.path.clone(),
                source,
            })?;
        let changed = text != self.applied;
        let store = self.secrets.clone();
        if !changed && store.is_none() {
            return Ok(false);
        }
        let mut config = if changed {
            self.registry
                .parse_with(&text, self.env.clone(), CheckMode::Lenient)?
        } else {
            (*self.current()).clone()
        };
        if let Some(store) = store {
            config.resolve_secrets(store.0.as_ref()).await?;
            if !changed && config.same_secrets(&self.current()) {
                return Ok(false);
            }
        }
        self.applied = text;
        self.publish(config);
        Ok(true)
    }

    fn publish(&mut self, config: Config) {
        let now = Instant::now();
        self.leases = config
            .secret_leases()
            .into_iter()
            .map(|lease| Tracked {
                due: now + lease.ttl * 2 / 3,
                lease,
            })
            .collect();
        self.tx.send_replace(Arc::new(config));
    }

    /// Работает до сигнала остановки: SIGHUP, [`ReloadTrigger`], опрос mtime с периодом `reload.poll_interval`
    /// и сроки аренд секретов.
    pub async fn run(mut self, shutdown: CancellationToken) {
        let mut hup = Hup::install();
        loop {
            let poll = self
                .current()
                .section::<ReloadConfig>("reload")
                .map(|c| c.poll_interval)
                .unwrap_or_default();
            let lease_due = self.leases.iter().map(|t| t.due).min();
            let wake = tokio::select! {
                () = shutdown.cancelled() => Wake::Stop,
                () = self.request.notified() => Wake::Forced,
                () = hup.recv() => Wake::Forced,
                () = sleep_or_never(poll) => Wake::Poll,
                () = sleep_until_or_never(lease_due) => Wake::Lease,
            };
            match wake {
                Wake::Stop => return,
                Wake::Lease => {
                    self.keep_leases().await;
                    continue;
                }
                Wake::Poll => {
                    if !self.file_changed().await {
                        continue;
                    }
                }
                Wake::Forced => {}
            }
            match self.reload().await {
                Ok(true) => tracing::info!(path = %self.path.display(), "configuration reloaded"),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "configuration reload failed; previous version kept");
                }
            }
        }
    }

    /// Продлевает наступившие аренды; истёкшая, отозванная или непродлеваемая аренда — секреты перечитываются.
    /// Неудача — повтор через шестую часть срока (не реже раза в секунду).
    async fn keep_leases(&mut self) {
        let Some(store) = self.secrets.clone() else {
            self.leases.clear();
            return;
        };
        let now = Instant::now();
        let retry = |lease: &SecretLease| now + (lease.ttl / 6).max(Duration::from_secs(1));
        let mut reread = false;
        for t in &mut self.leases {
            if t.due > now {
                continue;
            }
            if !t.lease.renewable {
                reread = true;
                continue;
            }
            match store.0.renew(&t.lease).await {
                Ok(renewed) => {
                    t.lease.ttl = renewed.ttl;
                    t.due = now + renewed.ttl * 2 / 3;
                }
                Err(Error::NotFound) => reread = true,
                Err(e) => {
                    tracing::warn!(lease = %t.lease.id, error = %e, "secret lease not renewed; retrying");
                    t.due = retry(&t.lease);
                }
            }
        }
        if !reread {
            return;
        }
        match self.reload().await {
            Ok(true) => tracing::info!("configuration secrets re-read after lease expiry"),
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, "configuration secrets re-read failed; previous version kept")
            }
        }
        // Не заменённые новой версией аренды — повторить позже, без частого цикла.
        for t in &mut self.leases {
            if t.due <= now {
                t.due = retry(&t.lease);
            }
        }
    }

    async fn file_changed(&mut self) -> bool {
        let stamp = tokio::fs::metadata(&self.path)
            .await
            .ok()
            .map(|m| stamp_of(&m));
        let changed = stamp != self.stamp;
        self.stamp = stamp;
        changed
    }
}

async fn sleep_until_or_never(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

fn stamp_of(m: &std::fs::Metadata) -> Stamp {
    (m.modified().ok(), m.len())
}

async fn sleep_or_never(period: Duration) {
    if period.is_zero() {
        std::future::pending::<()>().await;
    }
    tokio::time::sleep(period).await;
}

/// SIGHUP процесса; без обработчика (ошибка установки) — никогда не срабатывает.
struct Hup(Option<tp_sys::Signals>);

impl Hup {
    fn install() -> Self {
        match tp_sys::Signals::new() {
            Ok(s) => Self(Some(s)),
            Err(e) => {
                tracing::warn!(error = %e, "SIGHUP handler not installed; reload by poll only");
                Self(None)
            }
        }
    }

    async fn recv(&mut self) {
        let Some(signals) = &mut self.0 else {
            return std::future::pending().await;
        };
        // SIGTERM/SIGINT обрабатывает приложение (tokio рассылает сигнал всем подписчикам).
        loop {
            match signals.recv().await {
                Some(tp_sys::Signal::Hup) => return,
                Some(_) => {}
                None => return std::future::pending().await,
            }
        }
    }
}

/// Секция конфигурации для роли (`RoleCtx::config`) поверх рассылки [`Reloader`].
///
/// Каждая новая версия файла пересылается в канал секции (даже если эта секция не изменилась).
/// Пересылку ведёт задача tokio: она завершается, когда закрыт источник или удалены все получатели секции.
/// Секция `name` не зарегистрирована — `Error::NotFound`. Требует контекста tokio-рантайма.
pub fn section_watch(
    mut rx: watch::Receiver<Arc<Config>>,
    name: &str,
) -> tp_contracts::Result<ConfigRx> {
    let current: ConfigValue = rx
        .borrow_and_update()
        .section_any(name)
        .ok_or(Error::NotFound)?;
    let (tx, section_rx) = watch::channel(current);
    let name = name.to_owned();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = tx.closed() => return,
                changed = rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
            let section = rx.borrow_and_update().section_any(&name);
            if let Some(section) = section {
                tx.send_replace(section);
            }
        }
    });
    Ok(section_rx)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tp_contracts::role::RoleCtx;
    use tp_contracts::role::{CancellationToken, ReadyHandle};

    use super::*;

    #[derive(Debug, Default, Deserialize, JsonSchema, PartialEq)]
    #[serde(default, deny_unknown_fields)]
    struct LimitsConfig {
        max: u32,
    }

    const BASE: &str = "database: { url: pg }\nblob: { url: file:///b }\ndomain: d\n";

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str, text: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let path = std::env::temp_dir().join(format!(
                "tp-platform-{tag}-{}-{nanos}.yaml",
                std::process::id()
            ));
            fs::write(&path, text).expect("write");
            Self(path)
        }

        /// Пишет текст и сдвигает mtime, чтобы опрос заметил изменение при грубой точности ФС.
        fn rewrite(&self, text: &str, bump_secs: u64) {
            fs::write(&self.0, text).expect("write");
            let f = fs::File::options().write(true).open(&self.0).expect("open");
            f.set_modified(SystemTime::now() + Duration::from_secs(bump_secs))
                .expect("mtime");
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn registry() -> Arc<SectionRegistry> {
        let mut r = SectionRegistry::new();
        r.add_section::<LimitsConfig>("limits").expect("limits");
        Arc::new(r)
    }

    fn cfg(max: u32, poll_ms: u64) -> String {
        format!("{BASE}limits: {{ max: {max} }}\nreload: {{ poll_interval: {poll_ms}ms }}\n")
    }

    fn max(c: &Config) -> u32 {
        c.section::<LimitsConfig>("limits").expect("limits").max
    }

    #[tokio::test]
    async fn threshold_change_reaches_subscriber() {
        let file = TempFile::new("trigger", &cfg(1, 0));
        let reloader =
            Reloader::start_with(registry(), file.path().to_owned(), Vec::new()).expect("start");
        let mut rx = reloader.subscribe();
        let trigger = reloader.trigger();
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let task = tokio::spawn(reloader.run(shutdown));

        file.rewrite(&cfg(2, 0), 0);
        trigger.request();
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("in time")
            .expect("changed");
        assert_eq!(max(&rx.borrow_and_update()), 2);

        stop.cancel();
        task.await.expect("join");
    }

    #[tokio::test]
    async fn poll_detects_mtime_change() {
        let file = TempFile::new("poll", &cfg(1, 20));
        let reloader =
            Reloader::start_with(registry(), file.path().to_owned(), Vec::new()).expect("start");
        let mut rx = reloader.subscribe();
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let task = tokio::spawn(reloader.run(shutdown));

        file.rewrite(&cfg(7, 20), 2);
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("in time")
            .expect("changed");
        assert_eq!(max(&rx.borrow_and_update()), 7);

        stop.cancel();
        task.await.expect("join");
    }

    #[tokio::test]
    async fn broken_file_keeps_current_config() {
        let file = TempFile::new("broken", &cfg(3, 0));
        let mut reloader =
            Reloader::start_with(registry(), file.path().to_owned(), Vec::new()).expect("start");
        let rx = reloader.subscribe();

        assert_eq!(reloader.reload().await, Ok(false), "same text");
        file.rewrite(&format!("{BASE}limits: {{ max: many }}\n"), 0);
        let e = reloader.reload().await.expect_err("broken");
        assert!(
            matches!(&e, Error::Invalid(m) if m.starts_with("limits.max:")),
            "{e:?}"
        );
        file.rewrite("limits: [\n", 0);
        assert!(reloader.reload().await.is_err(), "syntax error");
        assert!(!rx.has_changed().expect("open"));
        assert_eq!(max(&reloader.current()), 3);

        file.rewrite(&cfg(4, 0), 0);
        assert_eq!(reloader.reload().await, Ok(true));
        assert_eq!(max(&rx.borrow()), 4);
    }

    #[tokio::test]
    async fn section_watch_for_role_ctx() {
        let file = TempFile::new("watch", &cfg(5, 0));
        let mut reloader =
            Reloader::start_with(registry(), file.path().to_owned(), Vec::new()).expect("start");
        assert!(section_watch(reloader.subscribe(), "nope").is_err());
        let mut ctx = RoleCtx {
            config: section_watch(reloader.subscribe(), "limits").expect("watch"),
            shutdown: CancellationToken::new(),
            drain_deadline: Duration::from_secs(1),
            ready: ReadyHandle::default(),
        };
        assert_eq!(ctx.config::<LimitsConfig>().expect("typed").max, 5);

        file.rewrite(&cfg(6, 0), 0);
        assert_eq!(reloader.reload().await, Ok(true));
        tokio::time::timeout(Duration::from_secs(5), ctx.config_changed())
            .await
            .expect("in time");
        assert_eq!(ctx.config::<LimitsConfig>().expect("typed").max, 6);
    }
}
