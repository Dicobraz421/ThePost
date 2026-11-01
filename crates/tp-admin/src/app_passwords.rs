//! Пароли приложений учётной записи (SRS 5.1, 6.8): выпуск, список, отзыв администратором.
//!
//! Пароль генерирует сервер (`tp_auth::AppPasswords`, argon2id): значение возвращается один раз в ответе на
//! выпуск (`Cache-Control: no-store`) и не хранится; в аудит тело не попадает. Отозванный пароль сразу перестаёт
//! приниматься `PasswordAuthenticator` (IMAP, SMTP, JMAP Basic). Предел действующих паролей — 409.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tp_contracts::app_password::AppPassword;
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, TenantId};
use utoipa::{OpenApi, ToSchema};

use crate::accounts::account;
use crate::server::{Admin, ApiResult, Problem};

/// Пароль приложения (без значения).
#[derive(Debug, Serialize, ToSchema)]
pub struct AppPasswordInfo {
    /// Идентификатор (для отзыва).
    pub id: String,
    /// Подпись.
    pub label: String,
    /// Выпущен.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
    /// Отозван; `null` — действует.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub revoked_at: Option<OffsetDateTime>,
}

impl From<AppPassword> for AppPasswordInfo {
    fn from(p: AppPassword) -> Self {
        Self {
            id: p.id,
            label: p.label,
            created_at: p.created_at,
            revoked_at: p.revoked_at,
        }
    }
}

/// Выпуск пароля.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewAppPassword {
    /// Подпись («Телефон», «Thunderbird дома»), до 255 символов.
    #[serde(default)]
    pub label: String,
}

/// Выпущенный пароль: значение показывается один раз.
#[derive(Debug, Serialize, ToSchema)]
pub struct IssuedAppPassword {
    /// Пароль.
    #[serde(flatten)]
    pub info: AppPasswordInfo,
    /// Значение пароля.
    pub password: String,
}

/// Маршруты паролей приложений.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route(
        "/api/v1/orgs/{org}/accounts/{account}/app-passwords",
        get(list).post(issue),
    )
    .route(
        "/api/v1/orgs/{org}/accounts/{account}/app-passwords/{id}",
        delete(revoke),
    )
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(list, issue, revoke),
    components(schemas(AppPasswordInfo, NewAppPassword, IssuedAppPassword))
)]
pub(crate) struct Doc;

async fn scope(a: &Admin, org: TenantId, id: AccountId) -> ApiResult<AccountScope> {
    account(a, org, id).await?;
    Ok(AccountScope {
        tenant: org,
        account: id,
    })
}

/// Пароли приложений учётной записи, включая отозванные.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/app-passwords", tag = "app-passwords",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    responses((status = 200, body = Vec<AppPasswordInfo>), (status = 404, body = Problem)))]
async fn list(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<Json<Vec<AppPasswordInfo>>> {
    let scope = scope(&a, org, id).await?;
    let list = a.d.app_passwords.list(scope).await?;
    Ok(Json(list.into_iter().map(AppPasswordInfo::from).collect()))
}

/// Выпускает пароль; значение — только в этом ответе.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/accounts/{account}/app-passwords", tag = "app-passwords",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    request_body = NewAppPassword,
    responses((status = 201, body = IssuedAppPassword), (status = 400, body = Problem),
        (status = 404, body = Problem), (status = 409, description = "предел действующих паролей", body = Problem)))]
async fn issue(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
    Json(body): Json<NewAppPassword>,
) -> ApiResult<Response> {
    let scope = scope(&a, org, id).await?;
    let (p, secret) = a.d.app_passwords.issue(scope, &body.label).await?;
    let issued = IssuedAppPassword {
        info: p.into(),
        password: secret.expose().to_owned(),
    };
    let mut resp = (StatusCode::CREATED, Json(issued)).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

/// Отзывает пароль; повторный отзыв — не ошибка.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/accounts/{account}/app-passwords/{id}", tag = "app-passwords",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)"),
        ("id" = String, Path, description = "Идентификатор пароля")),
    responses((status = 204), (status = 404, body = Problem)))]
async fn revoke(
    State(a): State<Arc<Admin>>,
    Path((org, id, password)): Path<(TenantId, AccountId, String)>,
) -> ApiResult<StatusCode> {
    let scope = scope(&a, org, id).await?;
    a.d.app_passwords.revoke(scope, &password).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::{StatusCode, header};
    use serde_json::{Value, json};
    use tp_auth::{Accounts, PasswordAuthenticator};
    use tp_contracts::auth::{Authenticator, Credentials};
    use tp_contracts::{AccountId, Error, TenantId};

    use crate::testenv::Env;

    /// Приёмка ADM-04: выпущенный пароль принимает AUTH-02, отозванный — нет.
    #[tokio::test]
    async fn revoked_password_is_rejected() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        let auth = PasswordAuthenticator::new(Accounts::new(
            TenantId::DEFAULT,
            Arc::new(env.meta.clone()),
        ))
        .with_app_passwords(env.app_passwords.clone());
        let login = |secret: &str| Credentials::Password {
            login: "user@example.org".into(),
            secret: secret.into(),
        };
        let base = format!(
            "/api/v1/orgs/{}/accounts/{}/app-passwords",
            TenantId::DEFAULT,
            scope.account
        );

        let body = Some(json!({"label": "Телефон"}));
        let (s, h, b) = env.call("POST", &base, Some("org"), body).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert_eq!(b["label"], "Телефон");
        assert_eq!(b["revoked_at"], Value::Null);
        let secret = b["password"].as_str().expect("password").to_owned();
        let id = b["id"].as_str().expect("id").to_owned();
        let p = auth
            .verify(login(&secret))
            .await
            .expect("действующий пароль");
        assert_eq!(p.account, scope.account);

        let (s, _, list) = env.call("GET", &base, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(list.as_array().expect("list").len(), 1);
        assert!(list[0].get("password").is_none() && list[0].get("hash").is_none());

        let (s, _, _) = env
            .call("DELETE", &format!("{base}/{id}"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert_eq!(auth.verify(login(&secret)).await, Err(Error::Forbidden));
        let (_, _, list) = env.call("GET", &base, Some("org"), None).await;
        assert!(list[0]["revoked_at"].is_string(), "{list}");
        let (s, _, _) = env
            .call("DELETE", &format!("{base}/{id}"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT, "повторный отзыв");
        let (s, _, _) = env
            .call("DELETE", &format!("{base}/nope"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        let log = format!("{:?}", env.audit.records());
        assert!(
            !log.contains(&secret),
            "значение пароля не попадает в аудит"
        );
        assert_eq!(env.audit.records().len(), 4);
    }

    #[tokio::test]
    async fn limits_and_access() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        let base = format!(
            "/api/v1/orgs/{}/accounts/{}/app-passwords",
            TenantId::DEFAULT,
            scope.account
        );
        let max = tp_auth::PasswordConfig::default().max_app_passwords;
        for i in 0..max {
            let body = Some(json!({ "label": format!("p{i}") }));
            let (s, _, _) = env.call("POST", &base, Some("org"), body).await;
            assert_eq!(s, StatusCode::CREATED);
        }
        let (s, _, _) = env.call("POST", &base, Some("org"), Some(json!({}))).await;
        assert_eq!(s, StatusCode::CONFLICT, "предел действующих паролей");
        let bad = Some(json!({"label": "a\nb"}));
        let (s, _, _) = env.call("POST", &base, Some("root"), bad).await;
        assert_eq!(s, StatusCode::CONFLICT, "предел проверяется до подписи");

        let (s, _, _) = env.call("POST", &base, Some("user"), Some(json!({}))).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let missing = format!(
            "/api/v1/orgs/{}/accounts/{}/app-passwords",
            TenantId::DEFAULT,
            AccountId::new()
        );
        let (s, _, _) = env.call("GET", &missing, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = env
            .call("POST", &missing, Some("org"), Some(json!({})))
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn invalid_label() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        let base = format!(
            "/api/v1/orgs/{}/accounts/{}/app-passwords",
            TenantId::DEFAULT,
            scope.account
        );
        let bad = Some(json!({"label": "a\nb"}));
        let (s, _, _) = env.call("POST", &base, Some("org"), bad).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }
}
