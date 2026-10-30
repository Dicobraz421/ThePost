//! Системный слой Linux: сигналы, лимиты ресурсов, сокеты, sandbox (seccomp) (SRS 2.2).

pub mod signals;

pub use signals::{Signal, Signals};
