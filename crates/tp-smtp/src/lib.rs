//! SMTP: приём, отправка, очередь, SPF/DKIM/DMARC/ARC, MTA-STS, DANE, Milter (SRS 4.1).
//!
//! - [`server`] — сессия SMTP (RFC 5321) на `smtp-proto`, роли smtp-in и submission, STARTTLS, `AUTH`;
//! - [`deliver`] — локальная доставка: разрешение получателей, `Received`, `MailStore::ingest`;
//! - [`config`] — секция `smtp` config.yaml.

pub mod config;
pub mod deliver;
pub mod server;

pub use config::SmtpConfig;
pub use deliver::{Delivery, Outbound};
pub use server::{Envelope, Handler, Mode, Reply, ServerDeps, Session, SessionCtx, SmtpService};
