//! Минимальные привилегии ролей (SRS 2.7, 6.8): профиль seccomp и ограничения Landlock на набор ролей узла.
//!
//! Профиль — объединение требований включённых ролей ([`Profile::for_roles`]):
//! - все роли: память, потоки, время, сигналы, epoll, сеть, чтение файлов;
//! - роли с данными (все, кроме smtp-in в режиме edge): запись файлов, каталоги BLOB, кэша и состояния;
//! - indexer: запуск подпроцессов извлечения текста (SYS-04).
//!
//! Landlock: процессу доступны только каталоги из [`RolePaths`] и служебные файлы [`ConfineConfig`];
//! узел edge — только конфигурация, секреты и служебные файлы (резолвер, procfs, cgroups; SRS 2.6).
//!
//! Применять после инициализации ролей, до приёма соединений: [`Confinement::apply`] действует на все
//! потоки процесса (seccomp — TSYNC; Landlock — TSYNC на ядрах с ABI v8, на старых — только вызывающий
//! поток и его будущие потомки) и необратимо. На ядре без Landlock — предупреждение, не отказ.
//! Режим seccomp `log` — для отладки профиля: вызовы вне профиля выполняются и пишутся в журнал аудита.

mod fs;
mod seccomp;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::role::{Role, RoleSet};

use fs::Grant;

/// Режим seccomp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SeccompMode {
    /// Вызовы вне профиля получают `EPERM`.
    #[default]
    Enforce,
    /// Вызовы вне профиля выполняются и пишутся в журнал аудита ядра (отладка профиля).
    Log,
    /// seccomp не применяется.
    Off,
}

/// Секция `confine`: ограничения процесса по ролям (SRS 2.7, 6.8); пропущенные поля — по умолчанию.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ConfineConfig {
    /// Режим seccomp.
    pub seccomp: SeccompMode,
    /// Ограничивать доступ к файловой системе через Landlock.
    pub landlock: bool,
    /// Служебные пути только для чтения для всех ролей: резолвер DNS, корневые сертификаты,
    /// procfs и cgroups (SYS-06). Несуществующие пропускаются.
    pub system_read: Vec<PathBuf>,
    /// Служебные пути для чтения и записи для всех ролей.
    pub system_rw: Vec<PathBuf>,
    /// Загрузчик и библиотеки для запуска подпроцессов (чтение и запуск); исполняемый файл
    /// процесса добавляется сам. Только роли с подпроцессами.
    pub spawn_exec: Vec<PathBuf>,
}

impl Default for ConfineConfig {
    fn default() -> Self {
        let paths = |list: &[&str]| list.iter().map(PathBuf::from).collect();
        Self {
            seccomp: SeccompMode::Enforce,
            landlock: true,
            system_read: paths(&[
                "/etc/resolv.conf",
                "/etc/hosts",
                "/etc/nsswitch.conf",
                "/etc/host.conf",
                "/etc/gai.conf",
                "/etc/ssl",
                "/proc/self",
                "/proc/meminfo",
                "/sys/fs/cgroup",
            ]),
            system_rw: paths(&["/dev/null"]),
            spawn_exec: paths(&[
                "/lib",
                "/lib64",
                "/usr/lib",
                "/usr/lib64",
                "/etc/ld.so.cache",
            ]),
        }
    }
}

/// Каталоги и файлы ролей узла; заполняет приложение по конфигурации.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RolePaths {
    /// Конфигурация (чтение), в т.ч. сертификаты TLS.
    pub config: Vec<PathBuf>,
    /// Файлы секретов, в т.ч. учётные данные входа в хранилище секретов (чтение).
    pub secrets: Vec<PathBuf>,
    /// Каталоги BLOB на локальном диске (чтение и запись).
    pub blob: Vec<PathBuf>,
    /// Кэш на NVMe: индексы, BLOB из S3 (чтение и запись).
    pub cache: Vec<PathBuf>,
    /// Прочее рабочее состояние: ACME, делегированная cgroup подпроцессов (чтение и запись).
    pub state: Vec<PathBuf>,
}

/// Требования ролей к системе.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Needs {
    /// Запись файлов и каталоги данных (BLOB, кэш, состояние).
    pub(crate) data: bool,
    /// Запуск подпроцессов.
    pub(crate) spawn: bool,
}

impl Needs {
    fn of(role: Role, edge: bool) -> Self {
        match role {
            Role::SmtpIn if edge => Needs::default(),
            Role::Indexer => Needs {
                data: true,
                spawn: true,
            },
            _ => Needs {
                data: true,
                spawn: false,
            },
        }
    }

    fn union(self, other: Self) -> Self {
        Needs {
            data: self.data || other.data,
            spawn: self.spawn || other.spawn,
        }
    }
}

/// Профиль ограничений набора ролей: объединение требований ролей.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    needs: Needs,
}

impl Profile {
    /// Профиль ролей узла; `set.edge` — smtp-in в режиме edge (SRS 2.6).
    pub fn for_roles(set: &RoleSet) -> Self {
        let needs = set
            .roles
            .iter()
            .map(|&role| Needs::of(role, set.edge))
            .fold(Needs::default(), Needs::union);
        Self { needs }
    }

    /// Роли пишут данные: запись файлов, каталоги BLOB, кэша и состояния.
    pub fn writes_data(&self) -> bool {
        self.needs.data
    }

    /// Роли запускают подпроцессы.
    pub fn spawns(&self) -> bool {
        self.needs.spawn
    }
}

/// Ошибка применения ограничений; запуск узла следует прервать.
#[derive(Debug, thiserror::Error)]
pub enum ConfineError {
    /// Сборка или установка фильтра seccomp.
    #[error("seccomp: {0}")]
    Seccomp(String),
    /// Правила Landlock.
    #[error("landlock: {0}")]
    Landlock(String),
}

/// Итог Landlock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandlockOutcome {
    /// Выключен в конфигурации.
    Disabled,
    /// Применён полностью; `all_threads` — ко всем потокам процесса (ABI v8).
    Enforced {
        /// Применён ко всем потокам.
        all_threads: bool,
    },
    /// Применён частично (ядро поддерживает не все права).
    Partial {
        /// Применён ко всем потокам.
        all_threads: bool,
    },
    /// Ядро не поддерживает Landlock: ограничений нет.
    Unsupported,
}

/// Итог применения ограничений.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfineReport {
    /// Режим seccomp.
    pub seccomp: SeccompMode,
    /// Итог Landlock.
    pub landlock: LandlockOutcome,
}

impl ConfineReport {
    /// Предупреждения: ограничения слабее запрошенных (для журнала и `thepost doctor`).
    pub fn warnings(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        match self.seccomp {
            SeccompMode::Enforce => {}
            SeccompMode::Log => {
                out.push("seccomp в режиме журнала: вызовы вне профиля не блокируются")
            }
            SeccompMode::Off => out.push("seccomp выключен"),
        }
        match self.landlock {
            LandlockOutcome::Disabled => out.push("Landlock выключен"),
            LandlockOutcome::Unsupported => {
                out.push("ядро не поддерживает Landlock: доступ к файловой системе не ограничен")
            }
            LandlockOutcome::Partial { .. } => {
                out.push("Landlock применён частично: ядро поддерживает не все права")
            }
            LandlockOutcome::Enforced { .. } => {}
        }
        if let LandlockOutcome::Enforced { all_threads: false }
        | LandlockOutcome::Partial { all_threads: false } = self.landlock
        {
            out.push("Landlock применён только к текущему потоку и его потомкам: ядро без ABI v8");
        }
        out
    }
}

/// Ограничения процесса для набора ролей.
#[derive(Debug, Clone)]
pub struct Confinement {
    cfg: ConfineConfig,
    profile: Profile,
    paths: RolePaths,
}

impl Confinement {
    /// Ограничения профиля `profile` с каталогами `paths`.
    pub fn new(cfg: ConfineConfig, profile: Profile, paths: RolePaths) -> Self {
        Self {
            cfg,
            profile,
            paths,
        }
    }

    /// Применяет Landlock, затем seccomp ко всем потокам процесса. Необратимо. Ошибка — запуск
    /// прервать (часть ограничений может уже действовать). Ослабления — в [`ConfineReport::warnings`]
    /// и в журнале.
    pub fn apply(&self) -> Result<ConfineReport, ConfineError> {
        self.apply_scoped(true)
    }

    /// `all_threads = false` — только вызывающий поток и его будущие потомки (тесты).
    pub(crate) fn apply_scoped(&self, all_threads: bool) -> Result<ConfineReport, ConfineError> {
        let programs = seccomp::filters(self.profile.needs, self.cfg.seccomp)?;
        let landlock = if self.cfg.landlock {
            fs::restrict(&self.rules(), all_threads)?
        } else {
            LandlockOutcome::Disabled
        };
        seccomp::install(&programs, all_threads)?;
        let report = ConfineReport {
            seccomp: self.cfg.seccomp,
            landlock,
        };
        for warning in report.warnings() {
            tracing::warn!("{warning}");
        }
        tracing::info!(
            seccomp = ?report.seccomp,
            landlock = ?report.landlock,
            writes_data = self.profile.writes_data(),
            spawns = self.profile.spawns(),
            "ограничения ролей применены"
        );
        Ok(report)
    }

    /// Правила Landlock по профилю.
    fn rules(&self) -> Vec<(Grant, Vec<PathBuf>)> {
        let p = &self.paths;
        let read = [
            p.config.as_slice(),
            p.secrets.as_slice(),
            self.cfg.system_read.as_slice(),
        ];
        let mut rules = vec![
            (Grant::Read, read.concat()),
            (Grant::Write, self.cfg.system_rw.clone()),
        ];
        if self.profile.needs.data {
            let data = [p.blob.as_slice(), p.cache.as_slice(), p.state.as_slice()];
            rules.push((Grant::Write, data.concat()));
        }
        if self.profile.needs.spawn {
            let mut exec = self.cfg.spawn_exec.clone();
            if let Ok(exe) = std::env::current_exe() {
                exec.push(exe);
            }
            rules.push((Grant::Exec, exec));
        }
        rules
    }
}
