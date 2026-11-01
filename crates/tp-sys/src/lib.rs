//! Системный слой Linux: сигналы, ресурсы узла, сокеты, sandbox (seccomp), ограничения ролей
//! (seccomp, Landlock) (SRS 2.2, 2.7).

pub mod confine;
pub mod resources;
pub mod sandbox;
pub mod signals;
pub mod socket;

pub use confine::{
    ConfineConfig, ConfineError, ConfineReport, Confinement, LandlockOutcome, Profile, RolePaths,
    SeccompMode,
};
pub use resources::{
    NodeResources, ResourceSnapshot, ResourcesConfig, watch_resources, watch_resources_in,
};
pub use signals::{Signal, Signals};
pub use socket::{ListenerConfig, bind_tcp};
