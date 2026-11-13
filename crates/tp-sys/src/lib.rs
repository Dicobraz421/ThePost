//! Системный слой Linux: сигналы, ресурсы узла, триггеры PSI, сокеты, место на диске, sandbox (seccomp),
//! ограничения ролей (seccomp, Landlock) (SRS 2.2, 2.7).

pub mod confine;
pub mod fs_usage;
pub mod psi;
pub mod resources;
pub mod sandbox;
pub mod signals;
pub mod socket;
#[cfg(test)]
mod test_logs;

pub use confine::{
    ConfineConfig, ConfineError, ConfineReport, Confinement, KRB5_RCACHE_DIR, LandlockOutcome,
    Profile, RolePaths, SeccompMode, landlock_abi,
};
pub use fs_usage::{FsUsage, fs_usage};
pub use psi::{PsiError, PsiFile, PsiStall, PsiThreshold, PsiTrigger};
pub use resources::{
    NodeResources, ResourceSnapshot, ResourcesConfig, watch_resources, watch_resources_in,
};
pub use signals::{Signal, Signals};
pub use socket::{ListenerConfig, adopt_listen_fd, bind_tcp};
