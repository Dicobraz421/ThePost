//! Межкрейтовые контракты: идентификаторы, ошибки, трейты хранилищ, сигналов, запросов узел → узел, поиска, аутентификации, журнала доставки, нагрузки, групп рассылки, настроек транспорта и правил потока почты, календарей и контактов, правил Sieve (SRS 2.1, 3, 4.1, 4.3, 4.4). Изменение — только PR класса B.
//!
//! Спецификация — `docs/CONTRACTS.md`. Асинхронные трейты для `dyn` — через `async-trait`; все трейты `Send + Sync + 'static`.

pub mod abuse;
pub mod acl;
pub mod addressbook;
pub mod admin_token;
pub mod app_password;
pub mod audit;
pub mod auth;
pub mod blob;
pub mod bus;
pub mod config;
pub mod dav;
pub mod delivery;
pub mod error;
pub mod events;
pub mod ext;
pub mod groups;
pub mod identity;
pub mod ids;
pub mod lease;
pub mod load;
pub mod mailstore;
pub mod membership;
pub mod meta;
pub mod queue;
pub mod retention;
pub mod role;
pub mod rpc;
pub mod sasl;
pub mod search;
pub mod secrets;
pub mod senders;
pub mod sieve;
pub mod submission;
pub mod tls;
pub mod transport;

pub use error::{Error, Result};
pub use ids::{
    AccountId, BlobHash, BlobKey, ContactId, DomainId, Gen, GroupId, HeldId, MailboxId, MessageId,
    ModSeq, OrgUnitId, QueueId, ShardId, TenantId, Uid, UidValidity,
};
