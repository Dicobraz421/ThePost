//! События ОС об изменении файлов конфигурации и сертификатов (SRS 2.8, закрытый список таймеров п. 5).
//!
//! Наблюдение ставится на каталог файла, а не на сам файл: Kubernetes заменяет ConfigMap и Secret атомарной
//! подменой символической ссылки `..data` в каталоге, файл по прежнему пути при этом не меняется. Получатель по
//! событию сверяет файлы сам (текст, mtime и размер), поэтому лишние события в каталоге безвредны.
//!
//! Источник событий — [`FsEvents`] (inotify; в тестах — источник в памяти). Если событий нет (источник
//! недоступен или наблюдение прекратилось), получатель переходит на запасной опрос с интервалом из своей секции.

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

/// Источник событий ОС процесса по умолчанию.
///
/// Привязка inotify требует системного вызова вне безопасного Rust: до появления привязки в зависимостях
/// (`tp-sys` или крейт `notify`) — [`NoFsEvents`], то есть запасной опрос. Приложение может передать свой
/// источник через [`crate::Reloader::with_fs_events`] и [`crate::TlsCerts::run_with`].
pub fn os_events() -> Arc<dyn FsEvents> {
    Arc::new(NoFsEvents)
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
    pub(crate) fn ensure(&mut self, dirs: Vec<PathBuf>, what: &str) {
        if dirs == self.dirs && (self.rx.is_some() || dirs.is_empty()) {
            return;
        }
        let retry = dirs == self.dirs;
        self.dirs = dirs;
        self.rx = None;
        match self.events.watch(&self.dirs) {
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

    #[tokio::test]
    async fn watch_falls_back_when_unavailable_or_stopped() {
        let mut none = DirWatch::new(Arc::new(NoFsEvents));
        none.ensure(vec![PathBuf::from("/etc")], "test");
        assert!(!none.active());

        let events = Arc::new(MemFsEvents::default());
        let mut watch = DirWatch::new(events.clone());
        watch.ensure(vec![PathBuf::from("/etc")], "test");
        watch.ensure(vec![PathBuf::from("/etc")], "test");
        assert_eq!(events.watches(), 1, "same dirs are watched once");
        assert!(watch.active());
        assert_eq!(events.fire(Path::new("/etc")), 1);
        watch.changed().await;
        assert!(watch.active());

        events.stop();
        watch.changed().await;
        assert!(!watch.active(), "stopped watch falls back to polling");
        watch.ensure(vec![PathBuf::from("/etc")], "test");
        assert!(watch.active(), "watch is re-established");
    }
}
