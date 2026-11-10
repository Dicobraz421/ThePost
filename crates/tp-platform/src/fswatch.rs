//! События ОС об изменении файлов конфигурации и сертификатов (SRS 2.8, закрытый список таймеров п. 5).
//!
//! Наблюдение ставится на каталог файла, а не на сам файл: Kubernetes заменяет ConfigMap и Secret атомарной
//! подменой символической ссылки `..data` в каталоге, файл по прежнему пути при этом не меняется. Получатель по
//! событию сверяет файлы сам (текст, mtime и размер), поэтому лишние события в каталоге безвредны.
//!
//! Источник событий — [`FsEvents`]: [`NotifyFsEvents`] (крейт `notify`, inotify на Linux; в тестах — источник в
//! памяти). Если событий нет (источник недоступен, например исчерпан `fs.inotify.max_user_watches`, или наблюдение
//! прекратилось), получатель пишет `warn` и переходит на запасной опрос с интервалом из своей секции.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;

/// Поток событий наблюдения: `()` на изменение в любом из каталогов; повторы до чтения склеиваются.
/// Закрытый канал — наблюдение прекратилось (получатель переходит на опрос).
pub type FsWatch = mpsc::Receiver<()>;

/// Источник событий ОС об изменении каталогов.
pub trait FsEvents: Send + Sync + fmt::Debug {
    /// Ставит наблюдение за каталогами `dirs`. Ошибка — события недоступны, получатель опрашивает файлы.
    fn watch(&self, dirs: &[PathBuf]) -> io::Result<FsWatch>;
}

/// Источник без событий: наблюдение всегда недоступно, получатели опрашивают файлы.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoFsEvents;

impl FsEvents for NoFsEvents {
    fn watch(&self, _dirs: &[PathBuf]) -> io::Result<FsWatch> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "inotify backend is not available in this build",
        ))
    }
}

/// События ОС через крейт `notify` (inotify на Linux), наблюдение без рекурсии.
///
/// Каждое наблюдение — свой экземпляр `notify` (на Linux — дескриптор inotify и поток чтения), он живёт, пока жив
/// [`FsWatch`]. Открытие и чтение файлов событием не считаются (иначе сверка файлов будила бы сама себя);
/// переполнение очереди ядра и ошибки чтения событий — событие (получатель сверит файлы). Удаление или переименование
/// наблюдаемого каталога — событие и затем прекращение наблюдения: получатель сверит файлы и поставит наблюдение
/// заново по новому набору каталогов. Требует среды выполнения tokio.
#[derive(Debug, Default, Clone, Copy)]
pub struct NotifyFsEvents;

impl FsEvents for NotifyFsEvents {
    fn watch(&self, dirs: &[PathBuf]) -> io::Result<FsWatch> {
        use notify::Watcher as _;

        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let roots = dirs
            .iter()
            .map(std::path::absolute)
            .collect::<io::Result<Vec<_>>>()?;
        let (tx, rx) = mpsc::channel(1);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let keep = tx.clone();
        let mut stop_tx = Some(stop_tx);
        let handler = move |event: notify::Result<notify::Event>| match event {
            Ok(event) => match classify(&event, &roots) {
                Change::Ignore => {}
                Change::Files => {
                    // Полный канал — событие уже ждёт чтения (склейка).
                    let _ = tx.try_send(());
                }
                Change::RootGone => {
                    let _ = tx.try_send(());
                    if let Some(stop) = stop_tx.take() {
                        let _ = stop.send(());
                    }
                }
            },
            Err(e) => {
                tracing::debug!(error = %e, "file change events error; rechecking files");
                let _ = tx.try_send(());
            }
        };
        let mut watcher =
            notify::RecommendedWatcher::new(handler, notify::Config::default()).map_err(to_io)?;
        for dir in dirs {
            watcher
                .watch(dir, notify::RecursiveMode::NonRecursive)
                .map_err(to_io)?;
        }
        // Наблюдение живёт, пока получатель не закрыт и наблюдаемые каталоги на месте.
        runtime.spawn(async move {
            let _watcher = watcher;
            tokio::select! {
                () = keep.closed() => {}
                _ = stop_rx => {}
            }
        });
        Ok(rx)
    }
}

/// Значение события `notify` для получателя.
#[derive(Debug, PartialEq, Eq)]
enum Change {
    /// Открытие или чтение без записи.
    Ignore,
    /// Изменение в каталоге (или неизвестное): сверить файлы.
    Files,
    /// Наблюдаемый каталог удалён или переименован: сверить файлы и поставить наблюдение заново.
    RootGone,
}

fn classify(event: &notify::Event, roots: &[PathBuf]) -> Change {
    use notify::EventKind;
    use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};

    match event.kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => Change::Files,
        EventKind::Access(_) => Change::Ignore,
        EventKind::Remove(_)
        | EventKind::Modify(ModifyKind::Name(RenameMode::From | RenameMode::Any))
            if event.paths.iter().any(|p| roots.contains(p)) =>
        {
            Change::RootGone
        }
        _ => Change::Files,
    }
}

fn to_io(e: notify::Error) -> io::Error {
    match e.kind {
        notify::ErrorKind::Io(io) => io,
        notify::ErrorKind::PathNotFound => io::Error::new(io::ErrorKind::NotFound, e.to_string()),
        notify::ErrorKind::MaxFilesWatch => io::Error::new(
            io::ErrorKind::QuotaExceeded,
            "inotify watch limit reached (fs.inotify.max_user_watches)",
        ),
        _ => io::Error::other(e.to_string()),
    }
}

/// Источник событий ОС процесса по умолчанию — [`NotifyFsEvents`]. Приложение может передать свой источник через
/// [`crate::Reloader::with_fs_events`] и [`crate::TlsCerts::run_with`].
pub fn os_events() -> Arc<dyn FsEvents> {
    Arc::new(NotifyFsEvents)
}

/// Каталоги наблюдения для файлов: каталог пути и каталог цели символической ссылки (без повторов).
/// Блокирующий I/O (разрешение ссылок).
pub(crate) fn dirs_of(files: &[&Path]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |dir: Option<&Path>| {
        let dir = match dir {
            Some(d) if d.as_os_str().is_empty() => PathBuf::from("."),
            Some(d) => d.to_owned(),
            None => return,
        };
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    for file in files {
        push(file.parent());
        if let Ok(real) = std::fs::canonicalize(file) {
            push(real.parent());
        }
    }
    dirs
}

/// Наблюдение за каталогами файлов получателя с переходом на опрос.
#[derive(Debug)]
pub(crate) struct DirWatch {
    events: Arc<dyn FsEvents>,
    /// Каталоги, для которых наблюдение поставлено или не удалось (повторно не ставится).
    dirs: Vec<PathBuf>,
    rx: Option<FsWatch>,
}

impl DirWatch {
    pub(crate) fn new(events: Arc<dyn FsEvents>) -> Self {
        Self {
            events,
            dirs: Vec::new(),
            rx: None,
        }
    }

    /// Ставит наблюдение за каталогами `dirs`, если набор изменился или прежнее наблюдение прекратилось.
    /// Постановка наблюдения — блокирующие системные вызовы, выполняется в `spawn_blocking`.
    pub(crate) async fn ensure(&mut self, dirs: Vec<PathBuf>, what: &str) {
        if dirs == self.dirs && (self.rx.is_some() || dirs.is_empty()) {
            return;
        }
        let retry = dirs == self.dirs;
        self.dirs = dirs;
        self.rx = None;
        let events = self.events.clone();
        let watch_dirs = self.dirs.clone();
        let result = tokio::task::spawn_blocking(move || events.watch(&watch_dirs))
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e)));
        match result {
            Ok(rx) => {
                tracing::debug!(what, dirs = ?self.dirs, "file change events enabled");
                self.rx = Some(rx);
            }
            Err(e) if retry => {
                tracing::debug!(what, error = %e, "file change events still unavailable");
            }
            Err(e) => {
                tracing::warn!(what, error = %e, "file change events unavailable; falling back to polling");
            }
        }
    }

    /// События ОС поступают: запасной опрос не нужен.
    pub(crate) fn active(&self) -> bool {
        self.rx.is_some()
    }

    /// Ждёт события. Прекращение наблюдения тоже считается событием (сверка и переход на опрос).
    /// Без наблюдения ждёт вечно.
    pub(crate) async fn changed(&mut self) {
        let Some(rx) = &mut self.rx else {
            return std::future::pending().await;
        };
        if rx.recv().await.is_none() {
            tracing::warn!(dirs = ?self.dirs, "file change events stopped; falling back to polling");
            self.rx = None;
            // Повторная попытка — при следующем `ensure` с тем же набором (не чаще периода опроса).
            self.dirs.clear();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Источник событий в памяти: [`MemFsEvents::fire`] — изменение в каталогах всех наблюдений.
    #[derive(Debug, Default)]
    pub(crate) struct MemFsEvents {
        watchers: Mutex<Vec<(Vec<PathBuf>, mpsc::Sender<()>)>>,
    }

    impl MemFsEvents {
        /// Событие для наблюдений, включающих каталог `dir`; число получателей.
        pub(crate) fn fire(&self, dir: &Path) -> usize {
            let watchers = self.watchers.lock().expect("lock");
            watchers
                .iter()
                .filter(|(dirs, _)| dirs.iter().any(|d| d == dir))
                .filter(|(_, tx)| tx.try_send(()).is_ok() || !tx.is_closed())
                .count()
        }

        /// Прекращает все наблюдения.
        pub(crate) fn stop(&self) {
            self.watchers.lock().expect("lock").clear();
        }

        /// Число поставленных наблюдений.
        pub(crate) fn watches(&self) -> usize {
            self.watchers.lock().expect("lock").len()
        }
    }

    impl FsEvents for MemFsEvents {
        fn watch(&self, dirs: &[PathBuf]) -> io::Result<FsWatch> {
            let (tx, rx) = mpsc::channel(1);
            self.watchers
                .lock()
                .expect("lock")
                .push((dirs.to_vec(), tx));
            Ok(rx)
        }
    }

    #[test]
    fn dirs_follow_symlink_target() {
        let base = std::env::temp_dir().join(format!("tp-platform-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("..2026_11_10");
        std::fs::create_dir_all(&data).expect("dir");
        std::fs::write(data.join("config.yaml"), "x").expect("write");
        let conf = base.join("conf");
        std::fs::create_dir_all(&conf).expect("dir");
        std::os::unix::fs::symlink(data.join("config.yaml"), conf.join("config.yaml"))
            .expect("symlink");

        let dirs = dirs_of(&[&conf.join("config.yaml"), &conf.join("other.yaml")]);
        let real = std::fs::canonicalize(&data).expect("canonical");
        assert_eq!(dirs, vec![conf.clone(), real]);
        assert_eq!(dirs_of(&[Path::new("config.yaml")])[0], PathBuf::from("."));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Временный каталог теста (удаляется при выходе).
    pub(crate) struct TempDir(pub(crate) PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let dir = std::env::temp_dir()
                .join(format!("tp-platform-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Вычитывает накопившиеся события: `true`, если пришло хотя бы одно за `quiet`.
    async fn drain(rx: &mut FsWatch, quiet: std::time::Duration) -> bool {
        let mut any = false;
        while let Ok(Some(())) = tokio::time::timeout(quiet, rx.recv()).await {
            any = true;
        }
        any
    }

    const DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
    const QUIET: std::time::Duration = std::time::Duration::from_millis(300);

    #[tokio::test]
    async fn notify_reports_writes_but_not_reads() {
        let dir = TempDir::new("notify-write");
        let file = dir.0.join("cert.pem");
        let mut rx = NotifyFsEvents
            .watch(std::slice::from_ref(&dir.0))
            .expect("inotify watch");

        std::fs::write(&file, "v1").expect("write");
        tokio::time::timeout(DEADLINE, rx.recv())
            .await
            .expect("event in time")
            .expect("watch alive");
        drain(&mut rx, QUIET).await;

        // Сверка файлов получателем (открытие и чтение) событием не считается.
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "v1");
        assert!(
            !drain(&mut rx, QUIET).await,
            "reads do not wake the receiver"
        );

        std::fs::write(&file, "v2").expect("write");
        assert!(drain(&mut rx, QUIET).await, "rewrite is reported");
    }

    #[tokio::test]
    async fn notify_stops_when_watched_dir_is_removed() {
        let base = TempDir::new("notify-gone");
        let dir = base.0.join("..v1");
        std::fs::create_dir_all(&dir).expect("dir");
        let mut rx = NotifyFsEvents
            .watch(std::slice::from_ref(&dir))
            .expect("inotify watch");

        std::fs::remove_dir_all(&dir).expect("remove");
        let closed =
            tokio::time::timeout(DEADLINE, async { while rx.recv().await.is_some() {} }).await;
        assert!(closed.is_ok(), "watch ends after its directory is removed");
    }

    #[tokio::test]
    async fn notify_missing_dir_is_an_error() {
        let base = TempDir::new("notify-missing");
        let err = NotifyFsEvents
            .watch(&[base.0.join("absent")])
            .expect_err("missing dir");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let mut watch = DirWatch::new(Arc::new(NotifyFsEvents));
        watch.ensure(vec![base.0.join("absent")], "test").await;
        assert!(!watch.active(), "falls back to polling");
        watch.ensure(vec![base.0.clone()], "test").await;
        assert!(watch.active());
    }

    #[tokio::test]
    async fn watch_falls_back_when_unavailable_or_stopped() {
        let mut none = DirWatch::new(Arc::new(NoFsEvents));
        none.ensure(vec![PathBuf::from("/etc")], "test").await;
        assert!(!none.active());

        let events = Arc::new(MemFsEvents::default());
        let mut watch = DirWatch::new(events.clone());
        watch.ensure(vec![PathBuf::from("/etc")], "test").await;
        watch.ensure(vec![PathBuf::from("/etc")], "test").await;
        assert_eq!(events.watches(), 1, "same dirs are watched once");
        assert!(watch.active());
        assert_eq!(events.fire(Path::new("/etc")), 1);
        watch.changed().await;
        assert!(watch.active());

        events.stop();
        watch.changed().await;
        assert!(!watch.active(), "stopped watch falls back to polling");
        watch.ensure(vec![PathBuf::from("/etc")], "test").await;
        assert!(watch.active(), "watch is re-established");
    }
}
