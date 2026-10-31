//! Кластер: шина событий (в процессе / Valkey), консистентное хэширование (SRS 2.1),
//! назначение разделов журнала событий по живым узлам.

pub mod hashing;
pub mod local_bus;
pub mod metrics;
pub mod partitions;
pub mod valkey_bus;

#[cfg(test)]
mod conformance;

pub use hashing::Rendezvous;
pub use local_bus::{LocalBus, LocalBusConfig};
pub use metrics::{BusCounters, BusMetrics};
pub use partitions::{AssignmentChange, PartitionAssignerConfig, RendezvousAssigner};
pub use valkey_bus::{ValkeyBus, ValkeyBusConfig, ValkeyConfig};
