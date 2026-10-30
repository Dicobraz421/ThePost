//! Пул изолированных подпроцессов для разбора недоверенных форматов (SRS 2.7, 4.6).
//!
//! Подпроцесс — подкоманда того же бинарного файла ([`WorkerCommand`]), вызывающая [`run_worker`].
//! Он сам ставит себе rlimit (память, CPU, без core) и seccomp (без сети, файлов и новых процессов),
//! затем обменивается заданиями с [`SandboxPool`] через stdin/stdout. Таймаут, аварийное завершение
//! или нарушение протокола — ошибка задания; подпроцесс уничтожается и заменяется новым при следующем
//! задании. Код без `unsafe`: системные вызовы — через безопасные обёртки nix и seccompiler.

mod filter;
mod pool;
mod protocol;
mod worker;

#[cfg(test)]
mod tests;

pub use pool::{JobError, SandboxConfig, SandboxPool, WorkerCommand};
pub use worker::run_worker;
