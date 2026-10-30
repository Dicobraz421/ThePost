//! Кластер: шина событий (в процессе / Valkey), консистентное хэширование (SRS 2.1).

pub mod hashing;
pub mod local_bus;
pub mod metrics;
pub mod valkey_bus;

#[cfg(test)]
mod conformance;

pub use hashing::Rendezvous;
pub use local_bus::{LocalBus, LocalBusConfig};
pub use metrics::{BusCounters, BusMetrics};
pub use valkey_bus::{ValkeyBus, ValkeyBusConfig};
