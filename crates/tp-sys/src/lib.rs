//! Системный слой Linux: сигналы, лимиты ресурсов, сокеты, sandbox (seccomp) (SRS 2.2).

pub mod resources;
pub mod sandbox;
pub mod signals;
pub mod socket;

pub use resources::{
    NodeResources, ResourceSnapshot, ResourcesConfig, watch_resources, watch_resources_in,
};
pub use signals::{Signal, Signals};
pub use socket::{ListenerConfig, bind_tcp};
