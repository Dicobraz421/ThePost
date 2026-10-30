//! Кластер: шина событий (в процессе / Valkey), консистентное хэширование (SRS 2.1).

pub mod hashing;

pub use hashing::Rendezvous;
