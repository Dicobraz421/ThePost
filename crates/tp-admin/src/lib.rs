//! Admin REST API: ящики, политики, экспорт/импорт, корзина, Legal Hold, аудит (SRS 5.2, 5.3).
//!
//! - [`server`] — каркас: HTTP (`axum`), OpenAPI (`utoipa`), вход Bearer OIDC, роли администраторов с областью,
//!   маршруты модулей, ошибки;
//! - [`audit`] — журнал аудита изменений.

pub mod audit;
pub mod server;
#[cfg(test)]
mod testenv;

pub use audit::{AuditRecord, AuditSink, LogAudit};
pub use server::{Admin, AdminConfig, AdminRole, Deps, RoleBinding};
