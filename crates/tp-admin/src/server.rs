//! Каркас Admin API (SRS 5.2): HTTP — `axum`, спецификация OpenAPI — `utoipa` (`GET /api/openapi.json`, без входа).
//!
//! Вход — только `Authorization: Bearer` (токен OpenID Connect, проверяет [`Authenticator`] — `tp-auth`
//! `OidcAuthenticator`); многофакторную аутентификацию обеспечивает поставщик удостоверений. Роли администраторов
//! назначает секция `admin` config.yaml по имени входа или группе каталога ([`RoleBinding`]):
//! - [`AdminRole::InstallationAdmin`] — все организации;
//! - [`AdminRole::OrganizationAdmin`] — только организация администратора.
//!
//! Ресурсы организации — под `/api/v1/orgs/{org}/...`: слой доступа ([`guard`]) проверяет вход, роль и область
//! роли до обработчика и пишет каждое изменение (не `GET`/`HEAD`), включая отказ, в журнал аудита
//! ([`crate::audit`]). Маршруты модулей (`AdminRoutes`, CONTRACTS.md#Extension points) — под тем же слоем,
//! префикс `/api/v1/modules/<имя>`. Ошибки — `application/problem+json` (RFC 9457) по `tp_contracts::Error`.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::RawPathParamsRejection;
use axum::extract::{DefaultBodyLimit, MatchedPath, RawPathParams, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Extension, Json, Router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use tp_contracts::auth::{Authenticator, Credentials, Principal};
use tp_contracts::ext::{AdminRequest, AdminRoutes};
use tp_contracts::mailstore::MailStore;
use tp_contracts::meta::{AccountStore, PolicyResolver};
use tp_contracts::retention::{Quota, TrashStore};
use tp_contracts::{Error, Result, TenantId};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi, ToSchema};

use crate::accounts::{DomainStore, PolicyStore};
use crate::audit::{AuditRecord, AuditSink};

/// Начало префикса маршрутов модуля.
pub const MODULE_PREFIX: &str = "/api/v1/modules/";

/// Роль администратора.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AdminRole {
    /// Администратор инсталляции: все организации.
    InstallationAdmin,
    /// Администратор организации: только своя организация.
    OrganizationAdmin,
}

/// Назначение роли: имена входа и группы каталога (`Principal::groups`), получающие роль.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoleBinding {
    /// Роль.
    pub role: AdminRole,
    /// Имена входа (без учёта регистра).
    #[serde(default)]
    pub logins: Vec<String>,
    /// Группы каталога.
    #[serde(default)]
    pub groups: Vec<String>,
}

/// Секция `admin` config.yaml (поля необязательны, неизвестное — ошибка). Без назначений ролей администраторов нет.
///
/// ```yaml
/// required_scope: thepost-admin
/// roles:
///   - role: installation_admin
///     groups: [mail-root]
///   - role: organization_admin
///     logins: [admin@example.org]
///     groups: [mail-admins]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AdminConfig {
    /// Область доступа токена OAuth, без которой ролей нет; `None` — не проверять.
    pub required_scope: Option<String>,
    /// Назначения ролей.
    pub roles: Vec<RoleBinding>,
    /// Размер страницы списков, если клиент не задал `limit`.
    pub page_size: usize,
    /// Наибольший `limit` страницы; больший урезается.
    pub max_page_size: usize,
    /// Наибольший размер тела запроса, байт.
    pub max_body_size: usize,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            required_scope: None,
            roles: Vec::new(),
            page_size: 100,
            max_page_size: 1_000,
            max_body_size: 1_048_576,
        }
    }
}

impl AdminConfig {
    /// Проверяет значения: страница от 1 до `max_page_size`, у назначения есть имя входа или группа.
    pub fn check(&self) -> Result<()> {
        if self.page_size == 0 || self.page_size > self.max_page_size {
            return Err(Error::invalid("admin: page_size — от 1 до max_page_size"));
        }
        if self
            .roles
            .iter()
            .any(|b| b.logins.is_empty() && b.groups.is_empty())
        {
            return Err(Error::invalid(
                "admin: у назначения роли нет ни logins, ни groups",
            ));
        }
        Ok(())
    }

    /// Размер страницы по запросу клиента.
    pub(crate) fn page(&self, limit: Option<usize>) -> usize {
        limit.unwrap_or(self.page_size).clamp(1, self.max_page_size)
    }
}

/// Администратор запроса: субъект и его роли (расширение запроса после [`guard`]).
#[derive(Debug, Clone)]
pub struct Caller {
    /// Субъект.
    pub principal: Principal,
    /// Роли; пусто — не администратор.
    pub roles: BTreeSet<AdminRole>,
}

impl Caller {
    /// Роль позволяет работать с организацией `tenant`.
    pub fn can_access(&self, tenant: TenantId) -> bool {
        self.roles.contains(&AdminRole::InstallationAdmin)
            || (self.roles.contains(&AdminRole::OrganizationAdmin)
                && tenant == self.principal.tenant)
    }
}

/// Зависимости Admin API: реализации контрактов, выбранные приложением.
#[derive(Clone)]
pub struct Deps {
    /// Проверка Bearer-токенов (OIDC).
    pub auth: Arc<dyn Authenticator>,
    /// Журнал аудита.
    pub audit: Arc<dyn AuditSink>,
    /// Маршруты модулей (`Registry::admin_routes`).
    pub modules: Vec<Arc<dyn AdminRoutes>>,
    /// Учётные записи.
    pub accounts: Arc<dyn AccountStore>,
    /// Использование квот.
    pub quota: Arc<dyn Quota>,
    /// Запись политик уровней.
    pub policies: Arc<dyn PolicyStore>,
    /// Действующие политики.
    pub resolver: Arc<dyn PolicyResolver>,
    /// Домены.
    pub domains: Arc<dyn DomainStore>,
    /// Корзина: список элементов.
    pub trash: Arc<dyn TrashStore>,
    /// Почтовое хранилище: восстановление из корзины с уведомлением клиентов.
    pub store: Arc<dyn MailStore>,
    /// Пароли приложений: выпуск, список, отзыв.
    pub app_passwords: tp_auth::AppPasswords,
}

impl fmt::Debug for Deps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps")
            .field(
                "modules",
                &self.modules.iter().map(|m| m.prefix()).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// Сервер Admin API; HTTP — [`Admin::router`].
#[derive(Debug)]
pub struct Admin {
    pub(crate) d: Deps,
    pub(crate) cfg: AdminConfig,
}

impl Admin {
    /// Сервер поверх зависимостей. Недопустимая конфигурация или префикс модуля — `Invalid`,
    /// повтор префикса — `Conflict`.
    pub fn new(d: Deps, cfg: AdminConfig) -> Result<Self> {
        cfg.check()?;
        let mut seen = BTreeSet::new();
        for m in &d.modules {
            let p = m.prefix();
            let name = p.strip_prefix(MODULE_PREFIX).unwrap_or_default();
            if name.is_empty() || name.contains(['/', '{', '}', '*']) {
                return Err(Error::invalid(format!(
                    "admin: префикс модуля {p:?} — не {MODULE_PREFIX}<имя>"
                )));
            }
            if !seen.insert(p) {
                return Err(Error::Conflict);
            }
        }
        Ok(Self { d, cfg })
    }

    /// Маршруты Admin API; TLS, слушатель (по умолчанию — внутренние адреса) — у вызывающего (`tp-platform`).
    pub fn router(self: Arc<Self>) -> Router {
        let mut api: Router<Arc<Admin>> = Router::new().route("/api/v1/whoami", get(whoami));
        api = crate::accounts::routes(api);
        api = crate::retention::routes(api);
        api = crate::app_passwords::routes(api);
        for m in &self.d.modules {
            let prefix = m.prefix().to_owned();
            let handler = {
                let m = m.clone();
                move |Extension(c): Extension<Caller>, method: Method, uri: Uri, body: Bytes| {
                    let m = m.clone();
                    async move { module_call(&*m, &c, method, &uri, &body).await }
                }
            };
            api = api
                .route(&prefix, any(handler.clone()))
                .route(&format!("{prefix}/{{*rest}}"), any(handler));
        }
        api.route_layer(middleware::from_fn_with_state(self.clone(), guard))
            .route("/api/openapi.json", get(openapi))
            .layer(DefaultBodyLimit::max(self.cfg.max_body_size))
            .with_state(self)
    }

    /// Субъект по заголовку `Authorization: Bearer`; нет заголовка, другая схема, неверный токен — `Forbidden`.
    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<Principal> {
        let value = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(Error::Forbidden)?;
        let (scheme, token) = value.split_once(' ').ok_or(Error::Forbidden)?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(Error::Forbidden);
        }
        self.d
            .auth
            .verify(Credentials::Bearer(token.trim().to_owned()))
            .await
    }

    /// Роли субъекта по назначениям конфигурации.
    pub fn caller(&self, principal: Principal) -> Caller {
        let scoped = self
            .cfg
            .required_scope
            .as_deref()
            .is_none_or(|s| principal.has_scope(s));
        let roles = self
            .cfg
            .roles
            .iter()
            .filter(|_| scoped)
            .filter(|b| {
                b.logins
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case(&principal.login))
                    || b.groups.iter().any(|g| principal.groups.contains(g))
            })
            .map(|b| b.role)
            .collect();
        Caller { principal, roles }
    }
}

/// Слой доступа: вход, роль, область роли по `{org}`; изменения — в журнал аудита.
async fn guard(
    State(a): State<Arc<Admin>>,
    params: std::result::Result<RawPathParams, RawPathParamsRejection>,
    mut req: Request,
    next: Next,
) -> Response {
    let principal = match a.authenticate(req.headers()).await {
        Ok(p) => p,
        Err(e) => return auth_error(&e),
    };
    let org = params.ok().and_then(|p| {
        p.iter()
            .find(|(k, _)| *k == "org")
            .map(|(_, v)| v.to_owned())
    });
    let tenant = match org.map(|o| o.parse::<TenantId>()).transpose() {
        Ok(t) => t,
        Err(e) => return ApiError(e).into_response(),
    };
    let caller = a.caller(principal);
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| path.clone(), |m| m.as_str().to_owned());
    let allowed = !caller.roles.is_empty() && tenant.is_none_or(|t| caller.can_access(t));
    let resp = if allowed {
        req.extensions_mut().insert(caller.clone());
        next.run(req).await
    } else {
        ApiError(Error::Forbidden).into_response()
    };
    if method != Method::GET && method != Method::HEAD {
        let record = AuditRecord {
            at: OffsetDateTime::now_utc(),
            tenant,
            actor_tenant: caller.principal.tenant,
            actor: caller.principal.account,
            actor_login: caller.principal.login.clone(),
            roles: caller.roles.iter().copied().collect(),
            method: method.to_string(),
            route,
            path,
            status: resp.status().as_u16(),
        };
        if let Err(e) = a.d.audit.record(&record).await {
            tracing::error!(error = %e, route = %record.route, "admin: запись аудита не сохранена");
        }
    }
    resp
}

fn auth_error(e: &Error) -> Response {
    if e.is_retryable() {
        return ApiError(e.clone()).into_response();
    }
    let mut resp = problem(StatusCode::UNAUTHORIZED, None);
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"admin\""),
    );
    resp
}

/// Вызов маршрута модуля: тело — JSON или пусто (`Null`).
async fn module_call(
    m: &dyn AdminRoutes,
    c: &Caller,
    method: Method,
    uri: &Uri,
    body: &[u8],
) -> ApiResult<Response> {
    let rest = uri.path().strip_prefix(m.prefix()).unwrap_or_default();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(body)
            .map_err(|e| Error::invalid(format!("тело запроса — не JSON: {e}")))?
    };
    let req = AdminRequest {
        method: method.to_string(),
        path: if rest.is_empty() {
            "/".into()
        } else {
            rest.into()
        },
        query: uri.query().map(str::to_owned),
        body,
    };
    let r = m.handle(&c.principal, req).await?;
    let status =
        StatusCode::from_u16(r.status).map_err(|_| Error::internal("код ответа модуля"))?;
    Ok(if r.body.is_null() {
        status.into_response()
    } else {
        (status, Json(r.body)).into_response()
    })
}

/// Ошибка обработчика: `tp_contracts::Error` → код HTTP и problem details.
#[derive(Debug)]
pub(crate) struct ApiError(pub Error);

/// Результат обработчика.
pub(crate) type ApiResult<T> = std::result::Result<T, ApiError>;

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}

/// Ошибка (RFC 9457).
#[derive(Debug, Serialize, ToSchema)]
pub struct Problem {
    /// Код HTTP.
    pub status: u16,
    /// Краткое описание кода.
    pub title: String,
    /// Подробность (недопустимые данные).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, detail) = match &self.0 {
            Error::NotFound => (StatusCode::NOT_FOUND, None),
            Error::Conflict => (StatusCode::CONFLICT, None),
            Error::Invalid(m) => (StatusCode::BAD_REQUEST, Some(m.clone())),
            Error::Forbidden => (StatusCode::FORBIDDEN, None),
            Error::QuotaExceeded => (StatusCode::INSUFFICIENT_STORAGE, None),
            Error::Unavailable { .. } => (StatusCode::SERVICE_UNAVAILABLE, None),
            Error::Internal(m) => {
                tracing::error!(error = %m, "admin: внутренняя ошибка");
                (StatusCode::INTERNAL_SERVER_ERROR, None)
            }
        };
        let mut resp = problem(status, detail);
        if let Error::Unavailable {
            retry_after: Some(d),
        } = self.0
        {
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(d.as_secs().max(1)));
        }
        resp
    }
}

fn problem(status: StatusCode, detail: Option<String>) -> Response {
    let body = Problem {
        status: status.as_u16(),
        title: status.canonical_reason().unwrap_or_default().to_owned(),
        detail,
    };
    let mut resp = (status, Json(body)).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    resp
}

/// Текущий администратор.
#[derive(Debug, Serialize, ToSchema)]
pub struct WhoAmI {
    /// Организация.
    pub tenant: String,
    /// Учётная запись.
    pub account: String,
    /// Имя входа.
    pub login: String,
    /// Роли.
    pub roles: Vec<AdminRole>,
}

/// Текущий администратор и его роли.
#[utoipa::path(get, path = "/api/v1/whoami", tag = "admin",
    responses((status = 200, body = WhoAmI), (status = 401, body = Problem), (status = 403, body = Problem)))]
async fn whoami(Extension(c): Extension<Caller>) -> Json<WhoAmI> {
    Json(WhoAmI {
        tenant: c.principal.tenant.to_string(),
        account: c.principal.account.to_string(),
        login: c.principal.login,
        roles: c.roles.into_iter().collect(),
    })
}

/// Спецификация OpenAPI встроенных маршрутов.
#[derive(Debug, OpenApi)]
#[openapi(
    info(title = "The Post Admin API"),
    paths(whoami),
    components(schemas(Problem, WhoAmI, AdminRole)),
    modifiers(&BearerAuth),
    security(("bearer" = []))
)]
pub struct ApiDoc;

struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, api: &mut utoipa::openapi::OpenApi) {
        api.components
            .get_or_insert_with(Default::default)
            .add_security_scheme(
                "bearer",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("JWT")
                        .build(),
                ),
            );
    }
}

/// Спецификация OpenAPI всех встроенных маршрутов.
pub fn openapi_spec() -> utoipa::openapi::OpenApi {
    let mut api = ApiDoc::openapi();
    api.merge(crate::accounts::Doc::openapi());
    api.merge(crate::retention::Doc::openapi());
    api.merge(crate::app_passwords::Doc::openapi());
    api
}

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(openapi_spec())
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;
    use tp_contracts::ext::AdminResponse;

    use super::*;
    use crate::testenv::{Env, config, subject};

    /// Модуль-эхо: возвращает запрос и имя входа.
    struct Echo;

    #[async_trait]
    impl AdminRoutes for Echo {
        fn prefix(&self) -> &str {
            "/api/v1/modules/echo"
        }

        async fn handle(&self, p: &Principal, req: AdminRequest) -> Result<AdminResponse> {
            Ok(AdminResponse {
                status: 200,
                body: serde_json::json!({
                    "method": req.method, "path": req.path, "query": req.query,
                    "body": req.body, "login": p.login,
                }),
            })
        }
    }

    fn echo_env() -> Env {
        Env::with(config(), vec![Arc::new(Echo)])
    }

    /// Приёмка ADM-01: изменение без роли — 403, с ролью — выполнено и записано в аудит.
    #[tokio::test]
    async fn change_needs_role_and_is_audited() {
        let env = echo_env();
        let path = "/api/v1/modules/echo/jobs/1?dry=1";
        let (s, _, _) = env
            .call("POST", path, Some("user"), Some(json!({"a": 1})))
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, b) = env
            .call("POST", path, Some("org"), Some(json!({"a": 1})))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            b,
            json!({"method": "POST", "path": "/jobs/1", "query": "dry=1", "body": {"a": 1},
                   "login": "admin@example.org"})
        );
        let (s, _, b) = env
            .call("GET", "/api/v1/modules/echo", Some("root"), None)
            .await;
        assert_eq!(
            (s, &b["path"], &b["body"]),
            (StatusCode::OK, &json!("/"), &Value::Null)
        );

        let log = env.audit.records();
        assert_eq!(log.len(), 2, "GET не пишется в аудит: {log:?}");
        assert_eq!(log[0].status, 403);
        assert!(log[0].roles.is_empty());
        assert_eq!(log[0].actor_login, "user@example.org");
        assert_eq!(log[1].status, 200);
        assert_eq!(log[1].roles, vec![AdminRole::OrganizationAdmin]);
        assert_eq!(log[1].route, "/api/v1/modules/echo/{*rest}");
        assert_eq!(log[1].path, "/api/v1/modules/echo/jobs/1");
        assert_eq!(log[1].actor_tenant, TenantId::DEFAULT);
        assert_eq!(log[1].tenant, None);
    }

    #[tokio::test]
    async fn bearer_only() {
        let env = Env::new();
        let (s, h, b) = env.call("GET", "/api/v1/whoami", None, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(h[header::WWW_AUTHENTICATE], "Bearer realm=\"admin\"");
        assert_eq!(h[header::CONTENT_TYPE], "application/problem+json");
        assert_eq!(b["status"], 401);
        let (s, _, _) = env.call("GET", "/api/v1/whoami", Some("nope"), None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let req = axum::http::Request::builder()
            .uri("/api/v1/whoami")
            .header(header::AUTHORIZATION, "Basic dXNlcjpwYXNz")
            .body(axum::body::Body::empty())
            .expect("request");
        let resp = tower::ServiceExt::oneshot(env.router.clone(), req)
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        env.auth.set_failure(Some(Error::UNAVAILABLE));
        let (s, _, _) = env.call("GET", "/api/v1/whoami", Some("root"), None).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn roles_by_login_and_group() {
        let env = Env::new();
        let (s, _, b) = env.call("GET", "/api/v1/whoami", Some("root"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["roles"], json!(["installation_admin"]));
        assert_eq!(b["login"], "root@example.org");
        let (_, _, b) = env.call("GET", "/api/v1/whoami", Some("org"), None).await;
        assert_eq!(b["roles"], json!(["organization_admin"]));
        let (s, _, _) = env.call("GET", "/api/v1/whoami", Some("user"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn required_scope() {
        let cfg = AdminConfig {
            required_scope: Some("thepost-admin".into()),
            ..config()
        };
        let env = Env::with(cfg, vec![]);
        let mut p = subject(TenantId::DEFAULT, "root@example.org", &["mail-root"]);
        p.scopes = Some(["mail".to_owned()].into());
        env.auth.add_token("narrow", p.clone());
        p.scopes = Some(["thepost-admin".to_owned()].into());
        env.auth.add_token("wide", p);
        let (s, _, _) = env
            .call("GET", "/api/v1/whoami", Some("narrow"), None)
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env.call("GET", "/api/v1/whoami", Some("wide"), None).await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn openapi_without_login() {
        let env = Env::new();
        let (s, _, b) = env.call("GET", "/api/openapi.json", None, None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(b["paths"]["/api/v1/whoami"]["get"].is_object(), "{b}");
        assert_eq!(
            b["components"]["securitySchemes"]["bearer"]["scheme"],
            "bearer"
        );
    }

    #[test]
    fn module_prefixes_and_config() {
        let deps = |modules: Vec<Arc<dyn AdminRoutes>>| Deps {
            modules,
            ..crate::testenv::deps()
        };
        let twice: Vec<Arc<dyn AdminRoutes>> = vec![Arc::new(Echo), Arc::new(Echo)];
        assert_eq!(
            Admin::new(deps(twice), config()).map(|_| ()),
            Err(Error::Conflict)
        );

        struct Bad;
        #[async_trait]
        impl AdminRoutes for Bad {
            fn prefix(&self) -> &str {
                "/api/v1/other"
            }
            async fn handle(&self, _: &Principal, _: AdminRequest) -> Result<AdminResponse> {
                Err(Error::NotFound)
            }
        }
        assert!(matches!(
            Admin::new(deps(vec![Arc::new(Bad)]), config()),
            Err(Error::Invalid(_))
        ));

        let cfg: AdminConfig = serde_json::from_value(json!({
            "roles": [{"role": "organization_admin", "groups": ["mail-admins"]}]
        }))
        .expect("config");
        assert_eq!(cfg.roles[0].role, AdminRole::OrganizationAdmin);
        assert_eq!(cfg.page_size, 100);
        assert!(serde_json::from_value::<AdminConfig>(json!({"rolez": []})).is_err());
        let empty: AdminConfig =
            serde_json::from_value(json!({"roles": [{"role": "installation_admin"}]}))
                .expect("config");
        assert!(empty.check().is_err());
    }
}
