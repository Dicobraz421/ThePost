//! IMAP4rev2 и POP3 (SRS 4.2).
//!
//! - [`proto`] — разбор команд и сериализация ответов IMAP4rev2 на `imap-codec`.
//! - [`session`] — сессия: состояния, TLS, аутентификация; [`Server`] — приём соединений и остановка с `BYE`.
//! - `mailbox` — операции с папками: LIST (LIST-EXTENDED, SPECIAL-USE, LIST-STATUS), SELECT, STATUS, CREATE,
//!   DELETE, RENAME, SUBSCRIBE.
//! - `fetch` — FETCH: метаданные, разделы BODY[], BINARY, частичная выборка.
//! - `store` — STORE и CONDSTORE (`UNCHANGEDSINCE`, `MODSEQ`).
//! - `search` — SEARCH и ESEARCH: метаданные и полнотекстовый индекс.
//! - `write` — APPEND, COPY, MOVE, EXPUNGE, CLOSE (UIDPLUS).
//! - `qresync` — QRESYNC: `SELECT … (QRESYNC …)`, `VANISHED`.
//! - `idle` — IDLE и уведомления о выбранной папке (`EXISTS`, `EXPUNGE`, `FETCH FLAGS`).
//! - `ext` — NAMESPACE (общие папки), QUOTA, ACL ([`Extensions`]).
//! - `compress` — `COMPRESS=DEFLATE`.
//! - [`pop3`] — POP3 (RFC 1939) со STLS и SASL; выключен по умолчанию.
//! - [`config`] — секция `imap` config.yaml.

mod compress;
pub mod config;
mod ext;
mod fetch;
#[doc(hidden)]
pub mod fuzz;
mod idle;
mod mailbox;
pub mod pop3;
pub mod proto;
mod qresync;
mod search;
pub mod session;
mod store;
mod write;

pub use config::ImapConfig;
pub use ext::Extensions;
pub use pop3::{Pop3Server, Pop3Session};
pub use session::{Deps, Server, Session, SessionCtx, Transport};

#[cfg(test)]
mod tests;
