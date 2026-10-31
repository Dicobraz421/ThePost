//! SMTP: приём, отправка, очередь, SPF/DKIM/DMARC/ARC, MTA-STS, DANE, Milter (SRS 4.1).
//!
//! - [`server`] — сессия SMTP (RFC 5321) на `smtp-proto`, роли smtp-in и submission, STARTTLS, `AUTH`;
//! - [`authcheck`] — SPF, DKIM, DMARC, ARC входящей почты на `mail-auth`;
//! - [`deliver`] — локальная доставка: разрешение получателей, `Received`, `MailStore::ingest`;
//! - [`config`] — секция `smtp` config.yaml.

pub mod authcheck;
pub mod config;
pub mod deliver;
pub mod server;

pub use authcheck::AuthCheck;
pub use config::SmtpConfig;
pub use deliver::{Delivery, Outbound};
pub use server::{Envelope, Handler, Mode, Reply, ServerDeps, Session, SessionCtx, SmtpService};
