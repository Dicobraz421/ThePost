//! Триггер PSI (Pressure Stall Information) для tokio: запись порога в файл давления и ожидание
//! события `POLLPRI` (SYS-09).
//!
//! Файл — `<ресурс>.pressure` cgroup v2 или `/proc/pressure/<ресурс>`. Ядро сообщает о превышении порога
//! не чаще раза за окно; триггер действует, пока открыт файл ([`PsiTrigger`] закрывает его при удалении).
//! Ядро без PSI (`CONFIG_PSI_DEFAULT_DISABLED` без `psi=1`, контейнер без файлов давления) —
//! [`PsiError::Unsupported`]: вызывающий переходит на перечитывание по интервалу.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

/// Границы окна триггера, которые принимает ядро (`kernel/sched/psi.c`).
const WINDOW_MIN: Duration = Duration::from_millis(500);
const WINDOW_MAX: Duration = Duration::from_secs(10);

/// Вид задержки триггера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PsiStall {
    /// Задержана хотя бы одна задача (`some`).
    #[default]
    Some,
    /// Задержаны все задачи, кроме простаивающих (`full`).
    Full,
}

/// Порог триггера: суммарная задержка `stall` за окно `window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PsiThreshold {
    /// Вид задержки.
    pub kind: PsiStall,
    /// Суммарная задержка за окно: `0 < stall <= window`.
    pub stall: Duration,
    /// Окно: от 500ms до 10s; без `CAP_SYS_RESOURCE` ядро принимает только окно, кратное 2s.
    pub window: Duration,
}

impl PsiThreshold {
    /// Порог `some`.
    pub fn some(stall: Duration, window: Duration) -> Self {
        Self {
            kind: PsiStall::Some,
            stall,
            window,
        }
    }

    /// Строка триггера для файла давления: `some <мкс> <мкс>`.
    pub fn line(&self) -> String {
        let kind = match self.kind {
            PsiStall::Some => "some",
            PsiStall::Full => "full",
        };
        format!(
            "{kind} {} {}",
            self.stall.as_micros(),
            self.window.as_micros()
        )
    }

    /// Порог в границах ядра: тогда `EINVAL` при записи означает отсутствие PSI, а не неверный порог.
    fn check(&self) -> Result<(), String> {
        let window_ok = (WINDOW_MIN..=WINDOW_MAX).contains(&self.window);
        if !window_ok || self.stall.is_zero() || self.stall > self.window {
            return Err(format!(
                "required 0 < stall <= window, window from 500ms to 10s, got {:?} / {:?}",
                self.stall, self.window
            ));
        }
        Ok(())
    }
}

/// Ошибка триггера PSI.
#[derive(Debug, thiserror::Error)]
pub enum PsiError {
    /// PSI недоступен: нет файла давления (`ENOENT`), ядро его не поддерживает или отвергло триггер
    /// (`EINVAL`, `EOPNOTSUPP`; без `CAP_SYS_RESOURCE` — окно не кратно 2s), файл нельзя ожидать
    /// через epoll. Перейти на интервал.
    #[error("PSI unavailable at {path}: {source}")]
    Unsupported {
        /// Файл давления.
        path: PathBuf,
        /// Ошибка ядра.
        source: io::Error,
    },
    /// Порог вне границ ядра.
    #[error("PSI threshold: {0}")]
    Invalid(String),
    /// Прочие ошибки открытия, записи или регистрации в tokio (права, нет среды выполнения tokio).
    #[error("PSI trigger at {path}: {source}")]
    Io {
        /// Файл давления.
        path: PathBuf,
        /// Ошибка.
        source: io::Error,
    },
    /// Триггер снят ядром (`POLLERR`: cgroup удалена) или ожидание прервано ошибкой.
    #[error("PSI trigger closed: {0}")]
    Closed(io::Error),
}

impl PsiError {
    /// PSI недоступен — перейти на перечитывание по интервалу.
    pub fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported { .. })
    }

    fn classify(path: &Path, source: io::Error) -> Self {
        let path = path.to_owned();
        let unsupported = matches!(
            source.raw_os_error(),
            Some(nix::libc::ENOENT | nix::libc::EINVAL | nix::libc::EOPNOTSUPP)
        );
        if unsupported {
            Self::Unsupported { path, source }
        } else {
            Self::Io { path, source }
        }
    }
}

/// Файл давления с записанным порогом, ещё не зарегистрированный в tokio; триггер действует, пока
/// файл открыт.
#[derive(Debug)]
pub struct PsiFile {
    path: PathBuf,
    file: File,
}

impl PsiFile {
    /// Открывает файл давления `path` и записывает порог. Блокирующий I/O: из async — через
    /// `spawn_blocking` или [`PsiTrigger::install`].
    pub fn open(path: &Path, threshold: &PsiThreshold) -> Result<Self, PsiError> {
        threshold.check().map_err(PsiError::Invalid)?;
        let mut file = File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| PsiError::classify(path, e))?;
        file.write_all(threshold.line().as_bytes())
            .map_err(|e| PsiError::classify(path, e))?;
        Ok(Self {
            path: path.to_owned(),
            file,
        })
    }

    /// Файл давления.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Регистрирует файл в реакторе tokio для ожидания `POLLPRI`. Вызывать в среде выполнения tokio.
    pub fn register(self) -> Result<PsiTrigger, PsiError> {
        let Self { path, file } = self;
        // SAFETY: дескриптор принадлежит `File` (владеющий тип); `File` перемещается внутрь `AsyncFd`
        // и не выдаётся наружу ни по значению, ни по `&mut`, поэтому дескриптор открыт и указывает на
        // тот же файл, пока `AsyncFd` зарегистрирован; при удалении `AsyncFd` снимает регистрацию до
        // закрытия `File`.
        #[allow(unsafe_code)]
        let registered = unsafe { AsyncFd::register_with_interest(file, Interest::PRIORITY) };
        match registered {
            Ok(fd) => Ok(PsiTrigger { path, fd }),
            // epoll не принимает файл (`EPERM`: обычный файл вместо файла давления).
            Err(e) => {
                let source = e.into_parts().1;
                Err(if source.raw_os_error() == Some(nix::libc::EPERM) {
                    PsiError::Unsupported { path, source }
                } else {
                    PsiError::Io { path, source }
                })
            }
        }
    }
}

/// Триггер PSI, зарегистрированный в tokio: [`PsiTrigger::wait`] ждёт события превышения порога.
#[derive(Debug)]
pub struct PsiTrigger {
    path: PathBuf,
    fd: AsyncFd<File>,
}

impl PsiTrigger {
    /// Ставит триггер на файл давления `path` и регистрирует его в tokio: открытие и запись —
    /// в `spawn_blocking`.
    pub async fn install(path: PathBuf, threshold: PsiThreshold) -> Result<Self, PsiError> {
        let opened = tokio::task::spawn_blocking({
            let path = path.clone();
            move || PsiFile::open(&path, &threshold)
        })
        .await
        .map_err(|e| PsiError::Io {
            path,
            source: io::Error::other(e),
        })??;
        opened.register()
    }

    /// Файл давления.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Ждёт следующего события превышения порога. Ошибка — триггер больше не сработает
    /// (снят ядром), вызывающий переходит на интервал.
    pub async fn wait(&self) -> Result<(), PsiError> {
        let mut guard = self
            .fd
            .ready(Interest::PRIORITY)
            .await
            .map_err(PsiError::Closed)?;
        if guard.ready().is_error() {
            return Err(PsiError::Closed(io::Error::other(
                "POLLERR: trigger removed by kernel",
            )));
        }
        guard.clear_ready();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn threshold() -> PsiThreshold {
        PsiThreshold::some(Duration::from_millis(150), Duration::from_secs(2))
    }

    #[test]
    fn threshold_line_and_bounds() {
        assert_eq!(threshold().line(), "some 150000 2000000");
        let full = PsiThreshold {
            kind: PsiStall::Full,
            ..threshold()
        };
        assert_eq!(full.line(), "full 150000 2000000");
        assert!(threshold().check().is_ok());
        for (stall, window) in [(0, 2000), (3000, 2000), (100, 400), (100, 11_000)] {
            let t = PsiThreshold::some(Duration::from_millis(stall), Duration::from_millis(window));
            assert!(t.check().is_err(), "{stall}/{window}");
        }
    }

    #[test]
    fn missing_file_is_unsupported() {
        let path = Path::new("/nonexistent-tp-sys/pressure/memory");
        let err = PsiFile::open(path, &threshold()).expect_err("нет файла");
        assert!(err.is_unsupported(), "{err}");
        let err = PsiFile::open(
            path,
            &PsiThreshold::some(Duration::ZERO, Duration::from_secs(2)),
        )
        .expect_err("порог");
        assert!(matches!(err, PsiError::Invalid(_)), "{err}");
    }

    /// Обычный файл вместо файла давления: запись проходит, epoll его не принимает — `Unsupported`.
    #[tokio::test]
    async fn regular_file_is_unsupported() {
        let path = std::env::temp_dir().join(format!("tp-sys-psi-{}", std::process::id()));
        std::fs::write(&path, b"").expect("write");
        let err = PsiTrigger::install(path.clone(), threshold())
            .await
            .expect_err("не файл давления");
        let _ = std::fs::remove_file(&path);
        assert!(err.is_unsupported(), "{err}");
    }

    /// На ядре с PSI триггер ставится и ожидание не завершается без давления; без PSI — `Unsupported`.
    #[tokio::test]
    async fn proc_pressure_trigger() {
        let path = PathBuf::from("/proc/pressure/memory");
        match PsiTrigger::install(path, threshold()).await {
            Ok(trigger) => {
                assert_eq!(trigger.path(), Path::new("/proc/pressure/memory"));
                let waited = tokio::time::timeout(Duration::from_millis(50), trigger.wait()).await;
                // Давление памяти в тесте маловероятно; событие — не ошибка.
                if let Ok(r) = waited {
                    r.expect("событие PSI");
                }
            }
            Err(e) => assert!(e.is_unsupported(), "{e}"),
        }
    }
}
