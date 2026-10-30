//! Межкрейтовые контракты: идентификаторы, ошибки, трейты хранилищ, шины, поиска, аутентификации (SRS 2.1, 3). Изменение — только PR класса B.
//!
//! Спецификация — `docs/CONTRACTS.md`. Асинхронные трейты для `dyn` — через `async-trait`; все трейты `Send + Sync + 'static`.

pub mod auth;
pub mod blob;
pub mod bus;
pub mod config;
pub mod error;
pub mod events;
pub mod ext;
pub mod ids;
pub mod lease;
pub mod mailstore;
pub mod membership;
pub mod meta;
pub mod role;
pub mod search;
pub mod tls;

pub use error::{Error, Result};
pub use ids::{
    AccountId, BlobHash, BlobKey, DomainId, Gen, MailboxId, MessageId, ModSeq, TenantId, Uid,
    UidValidity,
};
