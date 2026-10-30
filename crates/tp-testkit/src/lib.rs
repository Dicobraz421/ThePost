//! In-memory реализации контрактов и тестовый корпус писем для тестов полос.
//!
//! Реализации разделяют состояние между клонами (`Arc` внутри) и берут время из [`Clock`]:
//! тесты таймаутов используют [`ManualClock`]. Общие наборы тестов контрактов — [`suite`].

pub mod blob;
pub mod corpus;
pub mod infra;
pub mod membership;
pub mod meta;
pub mod secrets;
pub mod suite;

pub use blob::{BlobOp, MemBlobIndex, MemBlobStore};
pub use corpus::{LoadModel, SyntheticMail, corpus, pg_schema, pg_url};
pub use infra::{Clock, ManualClock, MemEventBus, MemEventLog, MemLease, SystemClock};
pub use membership::MemMembership;
pub use meta::MemMetadata;
pub use secrets::MemSecretStore;
