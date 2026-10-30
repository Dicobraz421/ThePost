//! Системный слой Linux: сигналы, лимиты ресурсов, сокеты, sandbox (seccomp) (SRS 2.2).

pub mod resources;
pub mod signals;

pub use resources::NodeResources;
pub use signals::{Signal, Signals};
