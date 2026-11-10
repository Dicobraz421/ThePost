//! Кластер: шина сигналов в процессе (один узел; в кластере — `tp_meta::signals::PgSignalBus`),
//! консистентное хэширование (SRS 2.1), назначение разделов журнала событий по составу кластера (SRS 2.8).

pub mod hashing;
pub mod local_bus;
pub mod metrics;
pub mod partitions;

#[cfg(test)]
mod conformance;

pub use hashing::Rendezvous;
pub use local_bus::{LocalBus, LocalBusConfig};
pub use metrics::{BusCounters, BusMetrics};
pub use partitions::{AssignmentChange, PartitionAssignerConfig, RendezvousAssigner};
