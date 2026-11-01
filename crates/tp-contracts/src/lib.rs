//! Межкрейтовые контракты: идентификаторы, ошибки, трейты хранилищ, шины, поиска, аутентификации, журнала доставки, нагрузки, групп рассылки (SRS 2.1, 3). Изменение — только PR класса B.
//!
//! Спецификация — `docs/CONTRACTS.md`. Асинхронные трейты для `dyn` — через `async-trait`; все трейты `Send + Sync + 'static`.

pub mod acl;
pub mod app_password;
pub mod auth;
pub mod blob;
pub mod bus;
pub mod config;
pub mod delivery;
pub mod error;
pub mod events;
pub mod ext;
pub mod groups;
pub mod ids;
pub mod lease;
pub mod load;
pub mod mailstore;
pub mod membership;
pub mod meta;
pub mod queue;
pub mod retention;
pub mod role;
pub mod sasl;
pub mod search;
pub mod secrets;
pub mod tls;

pub use error::{Error, Result};
pub use ids::{
    AccountId, BlobHash, BlobKey, DomainId, Gen, GroupId, HeldId, MailboxId, MessageId, ModSeq,
    QueueId, TenantId, Uid, UidValidity,
};
