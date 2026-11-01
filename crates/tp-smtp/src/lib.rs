//! SMTP: приём, отправка, очередь, SPF/DKIM/DMARC/ARC, MTA-STS, DANE, Milter (SRS 4.1).
//!
//! - [`server`] — сессия SMTP (RFC 5321) на `smtp-proto`, роли smtp-in и submission, STARTTLS, `AUTH`;
//! - [`authcheck`] — SPF, DKIM, DMARC, ARC входящей почты на `mail-auth`;
//! - [`deliver`] — локальная доставка: разрешение получателей, `Received`, `MailStore::ingest`;
//! - [`dkim_sign`] — DKIM-подпись исходящей почты;
//! - [`dsn`] — уведомления о доставке (RFC 3464);
//! - [`milter`] — фильтры Milter (антиспам, антивирус) после приёма письма;
//! - [`queue`] — очередь исходящей почты и роль smtp-out (отправка на MX через `mail-send`);
//! - [`config`] — секция `smtp` config.yaml.

pub mod authcheck;
pub mod config;
pub mod deliver;
pub mod dkim_sign;
pub mod dsn;
pub mod milter;
pub mod queue;
pub mod server;

pub use authcheck::AuthCheck;
pub use config::SmtpConfig;
pub use deliver::{Delivery, Outbound};
pub use milter::Milters;
pub use queue::{Queue, QueueWorker, SmtpOutService};
pub use server::{Envelope, Handler, Mode, Reply, ServerDeps, Session, SessionCtx, SmtpService};
