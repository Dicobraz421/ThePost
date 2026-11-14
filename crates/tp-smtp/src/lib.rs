//! SMTP: приём, отправка, очередь, SPF/DKIM/DMARC/ARC, MTA-STS, DANE, Milter (SRS 4.1).
//!
//! - [`server`] — сессия SMTP (RFC 5321) на `smtp-proto`, роли smtp-in и submission, STARTTLS, `AUTH`;
//! - [`address`] — адресация: подадреса `user+tag`, переписывание адресов конверта;
//! - [`authcheck`] — SPF, DKIM, DMARC, ARC входящей почты на `mail-auth`;
//! - [`deliver`] — локальная доставка: разрешение получателей, `Received`, `MailStore::ingest`;
//! - [`dkim_sign`] — DKIM-подпись исходящей почты;
//! - [`dsn`] — уведомления о доставке (RFC 3464);
//! - `fuzz` — проверки разборщиков для целей cargo-fuzz `crates/tp-smtp/fuzz` (SMTP-19; не API);
//! - [`forward`] — пересылка наружу: SRS-переписывание отправителя ([`srs`]) и ARC-подпись;
//! - [`groups`] — группы рассылки при приёме: ограничения отправителей, модерация, раскрытие;
//! - [`learn`] — обучение антиспама (Rspamd controller) по перемещению писем в «Спам» и из него;
//! - `itip` — приём iMIP (RFC 6047) при локальной доставке: приглашения и ответы — в `ItipDelivery`;
//! - [`limits`] — лимиты отправки учётной записи в час;
//! - [`metrics`] — метрики сессий и очереди (`/metrics`);
//! - [`milter`] — фильтры Milter (антиспам, антивирус) после приёма письма;
//! - [`policy`] — MTA-STS и DANE при отправке на MX;
//! - [`routes`] — политики доменов назначения организаций (обязательный TLS, закреплённый ключ, smart host,
//!   egress-IP) с перечитыванием по сигналу;
//! - [`sources`] — доверенные источники приёма: шлюз (`XCLIENT`, `XFORWARD`, результаты проверок шлюза) и
//!   приложения (отправка без SASL);
//! - [`sieve`] — правила Sieve при локальной доставке (`DeliveryRules`) и автоответ (RFC 3834);
//! - [`screen`] — экран соединений smtp-in (postscreen): задержка приветствия, конвейер, частота, кэш по IP;
//! - [`queue`] — очередь исходящей почты и роль smtp-out (отправка на MX через `mail-send`);
//! - [`srs`] — адреса SRS (Sender Rewriting Scheme) пересылки и их обратное преобразование;
//! - [`tracking`] — журнал доставки (message tracking);
//! - [`config`] — секция `smtp` config.yaml.

pub mod address;
pub mod authcheck;
pub mod config;
pub mod deliver;
pub mod dkim_sign;
pub mod dsn;
pub mod forward;
#[doc(hidden)]
pub mod fuzz;
pub mod groups;
mod itip;
pub mod learn;
pub mod limits;
pub mod metrics;
pub mod milter;
pub mod policy;
pub mod queue;
pub mod routes;
pub mod screen;
pub mod server;
pub mod sieve;
pub mod sources;
pub mod srs;
pub mod tracking;

pub use authcheck::AuthCheck;
pub use config::SmtpConfig;
pub use deliver::{Delivery, PartnerDomains};
pub use groups::Groups;
pub use limits::SendLimits;
pub use metrics::SmtpMetrics;
pub use milter::Milters;
pub use queue::{Queue, QueueWorker, SmtpOutService};
pub use server::{Envelope, Handler, Mode, Reply, ServerDeps, Session, SessionCtx, SmtpService};
pub use tracking::Tracking;
