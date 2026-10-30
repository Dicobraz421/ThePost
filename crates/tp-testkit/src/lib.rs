//! In-memory реализации контрактов и тестовый корпус писем для тестов полос.
//!
//! Реализации разделяют состояние между клонами (`Arc` внутри) и берут время из [`Clock`]:
//! тесты таймаутов используют [`ManualClock`].

pub mod blob;
pub mod infra;

pub use blob::{BlobOp, MemBlobIndex, MemBlobStore};
pub use infra::{Clock, ManualClock, MemEventBus, MemEventLog, MemLease, SystemClock};
