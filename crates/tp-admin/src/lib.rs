//! Admin REST API: ящики, политики, экспорт/импорт, корзина, Legal Hold, аудит (SRS 5.2, 5.3).
//!
//! - [`server`] — каркас: HTTP (`axum`), OpenAPI (`utoipa`), вход Bearer OIDC, роли администраторов с областью,
//!   маршруты модулей, ошибки;
//! - [`audit`] — журнал аудита изменений;
//! - [`accounts`] — организация, домены, учётные записи, квоты и политики уровней;
//! - [`retention`] — корзина ящика: список, восстановление, срок по политике.

pub mod accounts;
pub mod audit;
pub mod retention;
pub mod server;
#[cfg(test)]
mod testenv;

pub use accounts::{Domain, DomainStore, PolicyStore, PolicyTarget};
pub use audit::{AuditRecord, AuditSink, LogAudit};
pub use server::{Admin, AdminConfig, AdminRole, Deps, RoleBinding};
