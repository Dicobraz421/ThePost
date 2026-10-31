//! IMAP4rev2 и POP3 (SRS 4.2).
//!
//! - [`proto`] — разбор команд и сериализация ответов IMAP4rev2 на `imap-codec`.
//! - [`session`] — сессия: состояния, TLS, аутентификация; [`Server`] — приём соединений и остановка с `BYE`.
//! - [`config`] — секция `imap` config.yaml.

pub mod config;
pub mod proto;
pub mod session;

pub use config::ImapConfig;
pub use session::{Deps, Server, Session, SessionCtx, Transport};

#[cfg(test)]
mod tests;
