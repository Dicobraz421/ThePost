//! Сторона подпроцесса: ограничения (rlimit, seccomp) и цикл обработки заданий.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};

use nix::sys::resource::{Resource, UsageWho, getrusage, setrlimit};
use nix::sys::time::TimeValLike;

use super::{filter, protocol};

pub(crate) const ENV_MEMORY: &str = "TP_SANDBOX_MEMORY_BYTES";
pub(crate) const ENV_CPU: &str = "TP_SANDBOX_CPU_SECS";
pub(crate) const ENV_MAX_INPUT: &str = "TP_SANDBOX_MAX_INPUT_BYTES";

/// Лимиты, переданные пулом через окружение.
#[derive(Debug, Clone, Copy)]
struct Limits {
    memory_bytes: u64,
    cpu_secs: u64,
    max_input_bytes: u64,
}

impl Limits {
    fn from_env() -> io::Result<Self> {
        fn var(name: &str) -> io::Result<u64> {
            std::env::var_os(name)
                .as_deref()
                .and_then(OsStr::to_str)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{name} не задан: подпроцесс запускается только пулом sandbox"),
                    )
                })
        }
        Ok(Self {
            memory_bytes: var(ENV_MEMORY)?,
            cpu_secs: var(ENV_CPU)?,
            max_input_bytes: var(ENV_MAX_INPUT)?,
        })
    }

    /// Ограничения процесса. CPU: мягкий предел — два задания, жёсткий — на секунду больше
    /// (SIGXCPU, затем SIGKILL); после задания, исчерпавшего половину, подпроцесс уходит на замену.
    fn apply(&self) -> io::Result<()> {
        setrlimit(Resource::RLIMIT_CORE, 0, 0)?;
        if self.memory_bytes > 0 {
            setrlimit(Resource::RLIMIT_AS, self.memory_bytes, self.memory_bytes)?;
        }
        if self.cpu_secs > 0 {
            let soft = self.cpu_secs.saturating_mul(2);
            setrlimit(Resource::RLIMIT_CPU, soft, soft.saturating_add(1))?;
        }
        Ok(())
    }

    /// Остаток CPU меньше одного задания — подпроцесс надо заменить.
    fn should_retire(&self) -> bool {
        if self.cpu_secs == 0 {
            return false;
        }
        let Ok(usage) = getrusage(UsageWho::RUSAGE_SELF) else {
            return true;
        };
        let used_ms = usage.user_time().num_milliseconds() + usage.system_time().num_milliseconds();
        u64::try_from(used_ms).unwrap_or(u64::MAX) >= self.cpu_secs.saturating_mul(1000)
    }
}

/// Точка входа подкоманды-подпроцесса (`thepost sandbox-worker` или аналог).
///
/// Вызывать до запуска рантайма и потоков: устанавливает rlimit и seccomp (на все потоки),
/// затем обрабатывает задания из stdin, пока родитель не закроет его. `handler` получает
/// недоверенные данные; паника обработчика — ошибка задания. Возврат `Ok` — штатное завершение.
pub fn run_worker<F>(mut handler: F) -> io::Result<()>
where
    F: FnMut(&[u8]) -> Result<Vec<u8>, String>,
{
    let limits = Limits::from_env()?;
    limits.apply()?;
    let program = filter::worker_filter()?;
    seccompiler::apply_filter_all_threads(&program).map_err(io::Error::other)?;

    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    stdout.write_all(protocol::HANDSHAKE)?;
    stdout.flush()?;

    while let Some(input) = protocol::read_request(&mut stdin, limits.max_input_bytes)? {
        let result = catch_unwind(AssertUnwindSafe(|| handler(&input)))
            .unwrap_or_else(|_| Err("обработчик завершился паникой".to_owned()));
        let retire = limits.should_retire();
        protocol::write_response(&mut stdout, &result, retire)?;
        if retire {
            break;
        }
    }
    Ok(())
}
