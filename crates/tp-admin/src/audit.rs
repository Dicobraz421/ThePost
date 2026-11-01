//! Журнал аудита Admin API (SRS 5.2, 6.8): каждое изменение администратора — запись с организацией.
//!
//! Запись делает слой доступа [`crate::server`] после выполнения запроса, изменяющего данные (не `GET`/`HEAD`),
//! включая отказы в доступе. Тело запроса не записывается: в нём могут быть секреты.
//!
//! Куда пишется — [`AuditSink`]. Встроенная реализация [`LogAudit`] пишет запись в общий журнал событий
//! узла (`tracing`, цель `audit`). Хранилище только на добавление в PostgreSQL (META-22) подключается реализацией
//! этого трейта, экспорт в SIEM — [`crate::export`] (syslog RFC 5424 поверх TLS, OTLP), вместе —
//! [`crate::export::Fanout`].
//!
//! Чтение — `GET /api/v1/orgs/{org}/audit` ([`AuditLog`]): администраторы и аудитор организации, оператор — нет.
//! Контракта чтения журнала аудита пока нет: [`AuditLog`] — временный трейт полосы до CTR-25.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tp_contracts::{AccountId, Error, Result, TenantId};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::server::{Admin, AdminRole, ApiResult, Problem};

/// Шаблон маршрута чтения журнала аудита (единственный маршрут роли «аудитор»).
pub const AUDIT_ROUTE: &str = "/api/v1/orgs/{org}/audit";

/// Запись журнала аудита.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuditRecord {
    /// Момент (UTC).
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub at: OffsetDateTime,
    /// Организация, к которой относится операция; `None` — уровень инсталляции.
    #[schema(value_type = Option<String>)]
    pub tenant: Option<TenantId>,
    /// Организация администратора.
    #[schema(value_type = String)]
    pub actor_tenant: TenantId,
    /// Учётная запись администратора.
    #[schema(value_type = String)]
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

/// Запрос к журналу аудита организации.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditQuery {
    /// Начало интервала, включительно.
    pub since: Option<OffsetDateTime>,
    /// Конец интервала, не включительно.
    pub until: Option<OffsetDateTime>,
    /// Не более записей, ≥ 1.
    pub limit: usize,
    /// Продолжение после записи с этим номером.
    pub after: Option<u64>,
}

/// Запись журнала с порядковым номером.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct AuditEntry {
    /// Номер записи (возрастает в порядке добавления).
    pub seq: u64,
    /// Запись.
    #[serde(flatten)]
    pub record: AuditRecord,
}

/// Страница журнала аудита.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct AuditPage {
    /// Записи по возрастанию `seq`.
    pub entries: Vec<AuditEntry>,
    /// `after` следующей страницы; `null` — записей больше нет.
    pub next: Option<u64>,
}

/// Чтение журнала аудита (временный трейт полосы до контракта CTR-25).
#[async_trait]
pub trait AuditLog: Send + Sync + 'static {
    /// Записи организации `tenant` (без уровня инсталляции) по возрастанию `seq`: в интервале
    /// [`since`, `until`), после `after`, не более `limit`; `next` — номер последней, если страница полная.
    async fn search(&self, tenant: TenantId, query: &AuditQuery) -> Result<AuditPage>;
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

/// Параметры чтения журнала аудита.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct AuditParams {
    /// Начало интервала (RFC 3339), включительно.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub since: Option<OffsetDateTime>,
    /// Конец интервала (RFC 3339), не включительно.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub until: Option<OffsetDateTime>,
    /// Размер страницы (по умолчанию и наибольший — из конфигурации).
    pub limit: Option<usize>,
    /// Продолжение: `next` предыдущей страницы.
    pub after: Option<u64>,
}

/// Маршруты журнала аудита.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route(AUDIT_ROUTE, get(search))
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(paths(search), components(schemas(AuditRecord, AuditEntry, AuditPage)))]
pub(crate) struct Doc;

/// Журнал аудита организации.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/audit", tag = "audit",
    params(("org" = String, Path, description = "Организация (UUID)"), AuditParams),
    responses((status = 200, body = AuditPage), (status = 400, body = Problem), (status = 403, body = Problem),
        (status = 503, body = Problem)))]
async fn search(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Query(q): Query<AuditParams>,
) -> ApiResult<Json<AuditPage>> {
    if let (Some(s), Some(u)) = (q.since, q.until)
        && s >= u
    {
        return Err(Error::invalid("audit: since must be before until").into());
    }
    let log =
        a.d.audit_log
            .as_ref()
            .ok_or(Error::Unavailable { retry_after: None })?;
    let query = AuditQuery {
        since: q.since,
        until: q.until,
        limit: a.cfg.page(q.limit),
        after: q.after,
    };
    Ok(Json(log.search(org, &query).await?))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use crate::testenv::Env;

    fn audit() -> String {
        format!("/api/v1/orgs/{}/audit", tp_contracts::TenantId::DEFAULT)
    }

    /// Приёмка ADM-06: аудитор читает только журнал аудита, оператор — всё, кроме него, и только чтение.
    #[tokio::test]
    async fn auditor_and_operator_rights() {
        let env = Env::new();
        let org = format!("/api/v1/orgs/{}", tp_contracts::TenantId::DEFAULT);
        // Два изменения в журнал: отказ оператору и создание домена администратором.
        let domains = format!("{org}/domains");
        let body = Some(json!({"name": "example.org"}));
        let (s, _, _) = env
            .call("POST", &domains, Some("operator"), body.clone())
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env.call("POST", &domains, Some("org"), body).await;
        assert_eq!(s, StatusCode::CREATED);

        // Оператор: чтение организации и доменов — да; журнал аудита — нет.
        for path in [&org, &domains] {
            let (s, _, _) = env.call("GET", path, Some("operator"), None).await;
            assert_eq!(s, StatusCode::OK, "operator GET {path}");
        }
        let (s, _, _) = env.call("GET", &audit(), Some("operator"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // Аудитор: журнал аудита своей организации — да; остальное и чужая организация — нет.
        let (s, _, b) = env.call("GET", &audit(), Some("auditor"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let entries = b["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 2, "{b}");
        assert_eq!(entries[0]["status"], 403);
        assert_eq!(entries[0]["roles"], json!(["operator"]));
        assert_eq!(entries[1]["route"], "/api/v1/orgs/{org}/domains");
        assert!(entries[0]["seq"].as_u64() < entries[1]["seq"].as_u64());
        for path in [&org, &domains] {
            let (s, _, _) = env.call("GET", path, Some("auditor"), None).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "auditor GET {path}");
        }
        let (s, _, _) = env
            .call(
                "POST",
                &domains,
                Some("auditor"),
                Some(json!({"name": "a.example"})),
            )
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let other = format!("/api/v1/orgs/{}/audit", env.other_tenant);
        let (s, _, _) = env.call("GET", &other, Some("auditor"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, b) = env
            .call("GET", "/api/v1/whoami", Some("auditor"), None)
            .await;
        assert_eq!((s, &b["roles"]), (StatusCode::OK, &json!(["auditor"])));

        // Администратор организации читает журнал; страницы — по `after`.
        let (s, _, b) = env
            .call("GET", &format!("{}?limit=1", audit()), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["entries"].as_array().expect("entries").len(), 1);
        let next = b["next"].as_u64().expect("next");
        let (_, _, b) = env
            .call(
                "GET",
                &format!("{}?limit=10&after={next}", audit()),
                Some("org"),
                None,
            )
            .await;
        // После первой: создание домена (201) и отказ аудитору в изменении (403).
        assert_eq!(b["entries"][0]["status"], 201, "{b}");
        assert_eq!(b["entries"][1]["status"], 403, "{b}");
        assert!(b["next"].is_null());
        let bad = format!(
            "{}?since=2026-11-02T00:00:00Z&until=2026-11-01T00:00:00Z",
            audit()
        );
        let (s, _, _) = env.call("GET", &bad, Some("org"), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let ok = format!("{}?since=2000-01-01T00:00:00Z", audit());
        let (s, _, b) = env.call("GET", &ok, Some("auditor"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(!b["entries"].as_array().expect("entries").is_empty());
    }
}
