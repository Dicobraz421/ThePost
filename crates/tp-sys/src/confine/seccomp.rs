//! Профиль seccomp узла: белый список системных вызовов по группам, объединённым для набора ролей.
//!
//! Два фильтра (ядро применяет оба, действует самый строгий результат):
//! 1. `clone3` → `ENOSYS`: аргументы `clone3` лежат в памяти и не проверяются BPF, поэтому glibc
//!    переходит на `clone`, флаги которого проверяет второй фильтр (как профиль Docker по умолчанию);
//! 2. белый список: вне списка — `EPERM` (режим `enforce`) или запись в журнал аудита ядра (`log`).
//!
//! Без группы запуска подпроцессов `clone` разрешён только с `CLONE_THREAD` (потоки), `execve` нет.

use std::collections::BTreeMap;

use nix::libc;
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use super::{ConfineError, Needs, SeccompMode};

/// Все роли: память, потоки, синхронизация, время, сигналы, epoll, дескрипторы, чтение файлов.
const BASE: &[libc::c_long] = &[
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_readv,
    libc::SYS_writev,
    libc::SYS_pread64,
    libc::SYS_pwrite64,
    libc::SYS_preadv,
    libc::SYS_pwritev,
    libc::SYS_openat,
    libc::SYS_close,
    libc::SYS_close_range,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    libc::SYS_statx,
    libc::SYS_statfs,
    libc::SYS_fstatfs,
    libc::SYS_lseek,
    libc::SYS_getdents64,
    libc::SYS_readlinkat,
    libc::SYS_faccessat,
    libc::SYS_faccessat2,
    libc::SYS_getcwd,
    libc::SYS_fcntl,
    libc::SYS_ioctl,
    libc::SYS_flock,
    libc::SYS_dup,
    libc::SYS_dup3,
    libc::SYS_pipe2,
    libc::SYS_eventfd2,
    libc::SYS_epoll_create1,
    libc::SYS_epoll_ctl,
    libc::SYS_epoll_pwait,
    libc::SYS_epoll_pwait2,
    libc::SYS_ppoll,
    libc::SYS_pselect6,
    libc::SYS_brk,
    libc::SYS_mmap,
    libc::SYS_munmap,
    libc::SYS_mremap,
    libc::SYS_mprotect,
    libc::SYS_madvise,
    libc::SYS_msync,
    libc::SYS_membarrier,
    libc::SYS_futex,
    libc::SYS_set_robust_list,
    libc::SYS_get_robust_list,
    libc::SYS_set_tid_address,
    libc::SYS_rseq,
    libc::SYS_clone3,
    libc::SYS_sched_yield,
    libc::SYS_sched_getaffinity,
    libc::SYS_prctl,
    libc::SYS_prlimit64,
    libc::SYS_getrusage,
    libc::SYS_uname,
    libc::SYS_sysinfo,
    libc::SYS_clock_gettime,
    libc::SYS_clock_getres,
    libc::SYS_clock_nanosleep,
    libc::SYS_nanosleep,
    libc::SYS_gettimeofday,
    libc::SYS_getrandom,
    libc::SYS_getpid,
    libc::SYS_getppid,
    libc::SYS_gettid,
    libc::SYS_getuid,
    libc::SYS_geteuid,
    libc::SYS_getgid,
    libc::SYS_getegid,
    libc::SYS_getresuid,
    libc::SYS_getresgid,
    libc::SYS_getgroups,
    libc::SYS_tgkill,
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_sigaltstack,
    libc::SYS_restart_syscall,
    libc::SYS_exit,
    libc::SYS_exit_group,
];

/// Все роли: сокеты (PostgreSQL, Redis, S3, клиенты, журнал systemd).
const NET: &[libc::c_long] = &[
    libc::SYS_socket,
    libc::SYS_socketpair,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept,
    libc::SYS_accept4,
    libc::SYS_connect,
    libc::SYS_getsockname,
    libc::SYS_getpeername,
    libc::SYS_setsockopt,
    libc::SYS_getsockopt,
    libc::SYS_sendto,
    libc::SYS_recvfrom,
    libc::SYS_sendmsg,
    libc::SYS_recvmsg,
    libc::SYS_sendmmsg,
    libc::SYS_recvmmsg,
    libc::SYS_shutdown,
];

/// Роли с данными: запись файлов (BLOB, кэш, состояние).
const FS_WRITE: &[libc::c_long] = &[
    libc::SYS_fsync,
    libc::SYS_fdatasync,
    libc::SYS_ftruncate,
    libc::SYS_fallocate,
    libc::SYS_renameat2,
    libc::SYS_unlinkat,
    libc::SYS_mkdirat,
    libc::SYS_linkat,
    libc::SYS_symlinkat,
    libc::SYS_fchmod,
    libc::SYS_fchmodat,
    libc::SYS_utimensat,
    libc::SYS_copy_file_range,
    libc::SYS_sendfile,
];

/// Запуск подпроцессов (indexer: извлечение текста, SYS-04); `clone` — без ограничения флагов.
const SPAWN: &[libc::c_long] = &[
    libc::SYS_execve,
    libc::SYS_execveat,
    libc::SYS_wait4,
    libc::SYS_waitid,
    libc::SYS_kill,
    libc::SYS_pidfd_open,
    libc::SYS_pidfd_send_signal,
    libc::SYS_seccomp,
];

/// Вызовы, которых нет в asm-generic (aarch64, riscv64): старые варианты, используемые glibc и std на x86_64.
#[cfg(target_arch = "x86_64")]
const BASE_X86: &[libc::c_long] = &[
    libc::SYS_open,
    libc::SYS_stat,
    libc::SYS_lstat,
    libc::SYS_access,
    libc::SYS_readlink,
    libc::SYS_poll,
    libc::SYS_select,
    libc::SYS_pipe,
    libc::SYS_dup2,
    libc::SYS_epoll_wait,
    libc::SYS_epoll_create,
    libc::SYS_arch_prctl,
    libc::SYS_getdents,
    libc::SYS_time,
    libc::SYS_getrlimit,
];
#[cfg(not(target_arch = "x86_64"))]
const BASE_X86: &[libc::c_long] = &[];

#[cfg(target_arch = "x86_64")]
const FS_WRITE_X86: &[libc::c_long] = &[
    libc::SYS_rename,
    libc::SYS_renameat,
    libc::SYS_sync_file_range,
    libc::SYS_mkdir,
    libc::SYS_rmdir,
    libc::SYS_unlink,
    libc::SYS_link,
    libc::SYS_symlink,
    libc::SYS_chmod,
];
#[cfg(not(target_arch = "x86_64"))]
const FS_WRITE_X86: &[libc::c_long] = &[];

#[cfg(target_arch = "x86_64")]
const SPAWN_X86: &[libc::c_long] = &[libc::SYS_fork, libc::SYS_vfork];
#[cfg(not(target_arch = "x86_64"))]
const SPAWN_X86: &[libc::c_long] = &[];

/// Номера разрешённых вызовов для набора требований (без `clone`, у него отдельное правило).
pub(crate) fn allowed(needs: Needs) -> Vec<libc::c_long> {
    let mut list: Vec<libc::c_long> = [BASE, BASE_X86, NET].concat();
    if needs.data {
        list.extend_from_slice(FS_WRITE);
        list.extend_from_slice(FS_WRITE_X86);
    }
    if needs.spawn {
        list.extend_from_slice(SPAWN);
        list.extend_from_slice(SPAWN_X86);
    }
    list
}

fn err(e: impl std::fmt::Display) -> ConfineError {
    ConfineError::Seccomp(e.to_string())
}

/// Фильтры в порядке установки; пусто в режиме `off`.
pub(crate) fn filters(needs: Needs, mode: SeccompMode) -> Result<Vec<BpfProgram>, ConfineError> {
    let mismatch = match mode {
        SeccompMode::Off => return Ok(Vec::new()),
        SeccompMode::Enforce => SeccompAction::Errno(libc::EPERM as u32),
        SeccompMode::Log => SeccompAction::Log,
    };
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(err)?;

    let clone3 = SeccompFilter::new(
        BTreeMap::from([(libc::SYS_clone3, Vec::new())]),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        arch,
    )
    .map_err(err)?;

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = allowed(needs)
        .into_iter()
        .map(|nr| (nr, Vec::new()))
        .collect();
    let clone_rules = if needs.spawn {
        Vec::new()
    } else {
        let thread = libc::CLONE_THREAD as u64;
        let cond = SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::MaskedEq(thread),
            thread,
        )
        .map_err(err)?;
        vec![SeccompRule::new(vec![cond]).map_err(err)?]
    };
    rules.insert(libc::SYS_clone, clone_rules);
    let allow = SeccompFilter::new(rules, mismatch, SeccompAction::Allow, arch).map_err(err)?;

    Ok(vec![
        clone3.try_into().map_err(err)?,
        allow.try_into().map_err(err)?,
    ])
}

/// Устанавливает фильтры: `all_threads` — на все потоки процесса (TSYNC), иначе на вызывающий поток.
pub(crate) fn install(programs: &[BpfProgram], all_threads: bool) -> Result<(), ConfineError> {
    for program in programs {
        if all_threads {
            seccompiler::apply_filter_all_threads(program).map_err(err)?;
        } else {
            seccompiler::apply_filter(program).map_err(err)?;
        }
    }
    Ok(())
}
