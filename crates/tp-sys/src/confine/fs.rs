//! Landlock: доступ процесса только к каталогам ролей (крейт `landlock`, режим best-effort).

use std::path::PathBuf;

use landlock::{
    ABI, Access, AccessFs, BitFlags, RestrictSelfAttr, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus, path_beneath_rules,
};

use super::{ConfineError, LandlockOutcome};

/// Версия ABI, права которой ограничиваются. ABI v9 (`ResolveUnix`) не берём: он запрещал бы
/// подключение к Unix-сокетам (PostgreSQL, журнал systemd) вне разрешённых каталогов.
const ABI_HANDLED: ABI = ABI::V5;

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
