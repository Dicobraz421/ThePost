//! In-memory реализации контрактов и тестовый корпус писем для тестов полос.
//!
//! Реализации разделяют состояние между клонами (`Arc` внутри) и берут время из [`Clock`]:
//! тесты таймаутов используют [`ManualClock`].

pub mod infra;

pub use infra::{Clock, ManualClock, MemEventBus, MemEventLog, MemLease, SystemClock};
