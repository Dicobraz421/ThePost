//! Фильтр seccomp подпроцесса: белый список системных вызовов для разбора данных в памяти.
//! Нет `openat`, `socket`, `clone`, `execve` — ни сети, ни файлов, ни новых процессов и потоков;
//! чтение — только stdin, запись — только stdout/stderr. Прочие вызовы получают `EPERM`.

use std::collections::BTreeMap;
use std::io;

use nix::libc;
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// Вызовы без ограничения аргументов: память, синхронизация, время, сигналы, завершение.
const ALLOWED: &[libc::c_long] = &[
    libc::SYS_brk,
    libc::SYS_mmap,
    libc::SYS_munmap,
    libc::SYS_mremap,
    libc::SYS_mprotect,
    libc::SYS_madvise,
    libc::SYS_futex,
    libc::SYS_sched_yield,
    libc::SYS_clock_gettime,
    libc::SYS_getrandom,
    libc::SYS_getrusage,
    libc::SYS_getpid,
    libc::SYS_gettid,
    libc::SYS_tgkill,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_sigaltstack,
    libc::SYS_restart_syscall,
    libc::SYS_fstat,
    libc::SYS_lseek,
    libc::SYS_close,
    libc::SYS_exit,
    libc::SYS_exit_group,
];

fn fd_is(fd: u64) -> io::Result<SeccompRule> {
    let cond = SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, fd)
        .map_err(io::Error::other)?;
    SeccompRule::new(vec![cond]).map_err(io::Error::other)
}

/// Собирает BPF-программу для архитектуры процесса.
pub(crate) fn worker_filter() -> io::Result<BpfProgram> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        ALLOWED.iter().map(|&nr| (nr, Vec::new())).collect();
    rules.insert(libc::SYS_read, vec![fd_is(0)?]);
    rules.insert(libc::SYS_write, vec![fd_is(1)?, fd_is(2)?]);

    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(io::Error::other)?;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Errno(libc::EPERM as u32),
        SeccompAction::Allow,
        arch,
    )
    .map_err(io::Error::other)?;
    filter.try_into().map_err(io::Error::other)
}
