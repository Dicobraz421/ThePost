//! IMAP4rev2 и POP3 (SRS 4.2).
//!
//! - [`proto`] — разбор команд и сериализация ответов IMAP4rev2 на `imap-codec`.
//! - [`session`] — сессия: состояния, TLS, аутентификация; [`Server`] — приём соединений и остановка с `BYE`.
//! - `mailbox` — операции с папками: LIST (LIST-EXTENDED, SPECIAL-USE, LIST-STATUS), SELECT, STATUS, CREATE,
//!   DELETE, RENAME, SUBSCRIBE.
//! - `fetch` — FETCH: метаданные, разделы BODY[], BINARY, частичная выборка.
//! - `store` — STORE и CONDSTORE (`UNCHANGEDSINCE`, `MODSEQ`).
//! - `search` — SEARCH и ESEARCH: метаданные и полнотекстовый индекс.
//! - [`config`] — секция `imap` config.yaml.

pub mod config;
mod fetch;
mod mailbox;
pub mod proto;
mod search;
pub mod session;
mod store;

pub use config::ImapConfig;
pub use session::{Deps, Server, Session, SessionCtx, Transport};

#[cfg(test)]
mod tests;
