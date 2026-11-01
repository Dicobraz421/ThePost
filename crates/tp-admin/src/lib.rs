//! Admin REST API: ящики, политики, экспорт/импорт, корзина, Legal Hold, аудит (SRS 5.2, 5.3).
//!
//! - [`server`] — каркас: HTTP (`axum`), OpenAPI (`utoipa`), вход Bearer OIDC, роли администраторов с областью,
//!   маршруты модулей, ошибки;
//! - [`audit`] — журнал аудита изменений и его чтение;
//! - [`export`] — экспорт журнала аудита в SIEM: syslog (RFC 5424 поверх TLS) и OTLP;
//! - [`delivery`] — поиск в журнале доставки;
//! - [`groups`] — группы рассылки: ограничения, скрытие из GAL, очередь модерации;
//! - [`accounts`] — организация, домены, учётные записи, квоты и политики уровней;
//! - [`retention`] — корзина ящика: список, восстановление, срок по политике;
//! - [`app_passwords`] — пароли приложений: выпуск (значение один раз), список, отзыв.

pub mod accounts;
pub mod app_passwords;
pub mod audit;
pub mod delivery;
pub mod export;
pub mod groups;
pub mod retention;
pub mod server;
#[cfg(test)]
mod testenv;

pub use accounts::{Domain, DomainStore, PolicyStore, PolicyTarget};
pub use audit::{AuditEntry, AuditLog, AuditPage, AuditQuery, AuditRecord, AuditSink, LogAudit};
pub use export::{
    AuditExportConfig, Fanout, OtlpAudit, OtlpAuditConfig, SyslogAudit, SyslogConfig,
};
pub use groups::Moderation;
pub use server::{Admin, AdminConfig, AdminRole, Deps, RoleBinding};
