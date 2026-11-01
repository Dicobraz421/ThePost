//! Журнал аудита Admin API (SRS 5.2, 6.8): каждое изменение администратора — запись с организацией.
//!
//! Запись делает слой доступа [`crate::server`] после выполнения запроса, изменяющего данные (не `GET`/`HEAD`),
//! включая отказы в доступе. Тело запроса не записывается: в нём могут быть секреты.
//!
//! Куда пишется — [`AuditSink`]. Встроенная реализация [`LogAudit`] пишет запись в общий журнал событий
//! узла (`tracing`, цель `audit`): структурированный JSON и OTLP-экспорт телеметрии (`tp-platform`). Хранилище
//! только на добавление в PostgreSQL и экспорт в SIEM подключаются реализацией этого трейта (META-22, ADM-06).

use async_trait::async_trait;
use serde::Serialize;
use time::OffsetDateTime;
use tp_contracts::{AccountId, Result, TenantId};

use crate::server::AdminRole;

/// Запись журнала аудита.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditRecord {
    /// Момент (UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// Организация, к которой относится операция; `None` — уровень инсталляции.
    pub tenant: Option<TenantId>,
    /// Организация администратора.
    pub actor_tenant: TenantId,
    /// Учётная запись администратора.
    pub actor: AccountId,
    /// Имя входа администратора.
    pub actor_login: String,
    /// Роли администратора на момент операции; пусто — ролей нет (отказ).
    pub roles: Vec<AdminRole>,
    /// Метод HTTP.
    pub method: String,
    /// Шаблон маршрута (`/api/v1/orgs/{org}/domains`); неизвестный маршрут — путь.
    pub route: String,
    /// Путь запроса.
    pub path: String,
    /// Код ответа.
    pub status: u16,
}

/// Приёмник журнала аудита: только добавление.
#[async_trait]
pub trait AuditSink: Send + Sync + 'static {
    /// Добавляет запись.
    async fn record(&self, record: &AuditRecord) -> Result<()>;
}

/// Журнал аудита в журнале событий узла (`tracing`, цель `audit`, уровень `info`).
#[derive(Debug, Clone, Copy, Default)]
pub struct LogAudit;

#[async_trait]
impl AuditSink for LogAudit {
    async fn record(&self, r: &AuditRecord) -> Result<()> {
        let tenant = r.tenant.map(|t| t.to_string());
        tracing::info!(
            target: "audit",
            at = %r.at,
            tenant = tenant.as_deref(),
            actor_tenant = %r.actor_tenant,
            actor = %r.actor,
            actor_login = %r.actor_login,
            roles = ?r.roles,
            method = %r.method,
            route = %r.route,
            path = %r.path,
            status = r.status,
            "admin api"
        );
        Ok(())
    }
}
