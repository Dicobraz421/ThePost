//! Landlock: доступ процесса только к каталогам ролей (крейт `landlock`, режим best-effort).

use std::path::PathBuf;

use landlock::{
    ABI, Access, AccessFs, BitFlags, RestrictSelfAttr, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus, path_beneath_rules,
};

use nix::libc;

use super::{ConfineError, LandlockOutcome};

/// Флаг `LANDLOCK_CREATE_RULESET_VERSION` (`linux/landlock.h`): запрос версии ABI вместо набора правил.
const CREATE_RULESET_VERSION: libc::c_uint = 1;

/// Версия ABI, права которой ограничиваются. ABI v9 (`ResolveUnix`) не берём: он запрещал бы
/// подключение к Unix-сокетам (PostgreSQL, журнал systemd) вне разрешённых каталогов.
const ABI_HANDLED: ABI = ABI::V5;

/// Версия ABI Landlock ядра (`landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`) —
/// без `/sys/kernel/security`, которого нет в контейнерах и подах (SYS-11). `None` — ядро без
/// Landlock (`ENOSYS`) или Landlock выключен (`EOPNOTSUPP`); иные ошибки ядро не возвращает и они
/// тоже дают `None`. Ограничений не накладывает. В отличие от крейта `landlock`, возвращает версию
/// ядра как есть, в т.ч. новее известных крейту.
pub fn landlock_abi() -> Option<i32> {
    // SAFETY: с флагом `LANDLOCK_CREATE_RULESET_VERSION` ядро требует `attr == NULL` и `size == 0`,
    // по указателю не читает и не пишет, дескриптор не создаёт (возвращает номер версии), поэтому
    // вызов не затрагивает память и ресурсы процесса. Аргументы приведены к типам `long` системного
    // вызова.
    #[allow(unsafe_code)]
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0 as libc::size_t,
            CREATE_RULESET_VERSION,
        )
    };
    if v < 1 {
        return None;
    }
    i32::try_from(v).ok()
}

/// Право доступа к дереву каталогов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grant {
    /// Чтение файлов и каталогов.
    Read,
    /// Чтение и запуск (библиотеки и исполняемый файл подпроцессов).
    Exec,
    /// Чтение, запись, создание и удаление.
    Write,
}

impl Grant {
    fn access(self) -> BitFlags<AccessFs> {
        let read = AccessFs::ReadFile | AccessFs::ReadDir;
        match self {
            Grant::Read => read,
            Grant::Exec => read | AccessFs::Execute,
            Grant::Write => read | AccessFs::from_write(ABI_HANDLED),
        }
    }
}

fn err(e: impl std::fmt::Display) -> ConfineError {
    ConfineError::Landlock(e.to_string())
}

/// Ограничивает доступ правилами `rules`; несуществующие пути пропускаются.
/// `all_threads` — на все потоки процесса (ABI v8), иначе на вызывающий поток и его будущих потомков.
pub(crate) fn restrict(
    rules: &[(Grant, Vec<PathBuf>)],
    all_threads: bool,
) -> Result<LandlockOutcome, ConfineError> {
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(ABI_HANDLED))
        .map_err(err)?
        .create()
        .map_err(err)?;
    for (grant, paths) in rules {
        ruleset = ruleset
            .add_rules(path_beneath_rules(paths, grant.access()))
            .map_err(err)?;
    }
    let status = ruleset
        .all_threads(all_threads)
        .map_err(err)?
        .restrict_self()
        .map_err(err)?;
    Ok(outcome(&status.ruleset, status.all_threads))
}

/// Итог применения по статусу ядра.
pub(crate) fn outcome(status: &RulesetStatus, all_threads: bool) -> LandlockOutcome {
    match status {
        RulesetStatus::FullyEnforced => LandlockOutcome::Enforced { all_threads },
        RulesetStatus::PartiallyEnforced => LandlockOutcome::Partial { all_threads },
        RulesetStatus::NotEnforced => LandlockOutcome::Unsupported,
    }
}
