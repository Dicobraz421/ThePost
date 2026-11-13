//! Разбор запроса для всех маршрутов Admin API: `Json`, `Query`, `Path` поверх одноимённых извлекателей `axum` с
//! одним отказом — 400 `application/problem+json`, в `detail` — поле и причина (путь до поля — `serde_path_to_error`
//! внутри `axum`). Слишком большое тело — код `axum` (413), тоже problem+json. `Json` — и ответ: тело JSON.
//! Обработчик, который читает тело сам (необязательное тело `POST /api/v1/session`), разбирает его [`json_body`] —
//! с тем же отказом.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::server::Refused;

/// Тело JSON запроса или ответа.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Json<T>(pub T);

/// Параметры строки запроса.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Query<T>(pub T);

/// Параметры пути.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Path<T>(pub T);

/// Причина отказа: текст ошибки под оболочкой `axum` (`поле: причина`), без английского заголовка оболочки.
fn cause(e: &(dyn std::error::Error + 'static)) -> String {
    e.source()
        .map_or_else(|| e.to_string(), ToString::to_string)
}

/// Отказ разбора: 400 с пояснением.
fn invalid(what: &str, detail: String) -> Refused {
    Refused::Problem(StatusCode::BAD_REQUEST, format!("{what}: {detail}"))
}

/// Отказ разбора тела JSON: код и пояснение.
fn json_refusal(e: JsonRejection) -> (StatusCode, String) {
    let bad = |what: &str, detail: String| (StatusCode::BAD_REQUEST, format!("{what}: {detail}"));
    match e {
        JsonRejection::JsonDataError(e) => bad("тело запроса", cause(&e)),
        JsonRejection::JsonSyntaxError(e) => bad("тело запроса — не JSON", cause(&e)),
        JsonRejection::MissingJsonContentType(_) => bad(
            "тело запроса",
            "нужен заголовок Content-Type: application/json".into(),
        ),
        JsonRejection::BytesRejection(e) if e.status() != StatusCode::BAD_REQUEST => {
            (e.status(), e.body_text())
        }
        e => bad("тело запроса", e.body_text()),
    }
}

/// Тело JSON из прочитанных байтов (без проверки `Content-Type`): отказ — как у [`Json`], код и пояснение.
pub(crate) fn json_body<T: DeserializeOwned>(
    bytes: &[u8],
) -> std::result::Result<T, (StatusCode, String)> {
    axum::Json::<T>::from_bytes(bytes)
        .map(|axum::Json(v)| v)
        .map_err(json_refusal)
}

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for Json<T> {
    type Rejection = Refused;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        axum::Json::<T>::from_request(req, state)
            .await
            .map(|axum::Json(v)| Self(v))
            .map_err(|e| {
                let (status, detail) = json_refusal(e);
                Refused::Problem(status, detail)
            })
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

impl<T: DeserializeOwned, S: Send + Sync> FromRequestParts<S> for Query<T> {
    type Rejection = Refused;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(v)) => Ok(Self(v)),
            Err(QueryRejection::FailedToDeserializeQueryString(e)) => {
                Err(invalid("параметры запроса", cause(&e)))
            }
            Err(e) => Err(invalid("параметры запроса", e.body_text())),
        }
    }
}

impl<T: DeserializeOwned + Send, S: Send + Sync> FromRequestParts<S> for Path<T> {
    type Rejection = Refused;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(v)) => Ok(Self(v)),
            Err(PathRejection::FailedToDeserializePathParams(e)) => {
                Err(invalid("путь запроса", e.kind().to_string()))
            }
            Err(e) => Err(Refused::Problem(e.status(), e.body_text())),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{StatusCode, header};
    use serde_json::json;
    use tp_contracts::{AccountId, TenantId};

    use crate::init::SESSION_ROUTE;
    use crate::testenv::{Env, config};

    /// Приёмка ADM-43: ошибки разбора тела, строки и пути запроса — 400 problem+json с полем в `detail`.
    #[tokio::test]
    async fn parse_errors_are_problems() {
        let env = Env::new();
        let org = format!("/api/v1/orgs/{}", TenantId::DEFAULT);
        let cases = [
            (
                "PUT",
                format!("{org}/policy"),
                Some(json!({"trash_retention": 20})),
                "trash_retention",
            ),
            (
                "PUT",
                format!("{org}/policy"),
                Some(json!({"quota": 1})),
                "quota",
            ),
            ("GET", format!("{org}/audit?since=вчера"), None, "since"),
            ("GET", format!("{org}/accounts?limit=много"), None, "limit"),
            (
                "GET",
                format!("{org}/accounts/not-a-uuid"),
                None,
                "not-a-uuid",
            ),
            (
                "PATCH",
                format!("{org}/accounts/{}", AccountId::new()),
                Some(json!({"aliases": "a@example.org"})),
                "aliases",
            ),
            (
                "PATCH",
                format!("{org}/accounts/{}", AccountId::new()),
                Some(json!({"aliases": [1]})),
                "aliases",
            ),
        ];
        for (method, path, body, field) in cases {
            let (s, h, b) = env.call(method, &path, Some("root"), body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{method} {path}: {b}");
            assert_eq!(
                h[header::CONTENT_TYPE],
                "application/problem+json",
                "{path}"
            );
            assert_eq!(b["status"], 400, "{path}");
            let detail = b["detail"].as_str().unwrap_or_default();
            assert!(detail.contains(field), "{path}: {detail}");
        }
    }

    /// Организации нет — 404 problem+json от слоя доступа и на маршрутах с телом (`PATCH` псевдонимов): тело не
    /// разбирается и не проверяется.
    #[tokio::test]
    async fn missing_org_before_body() {
        let env = Env::new();
        let ghost = format!(
            "/api/v1/orgs/{}/accounts/{}",
            TenantId::new(),
            AccountId::new()
        );
        for body in [json!({"aliases": ["a@example.org"]}), json!({"aliases": 1})] {
            let (s, h, b) = env.call("PATCH", &ghost, Some("root"), Some(body)).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
            assert_eq!(h[header::CONTENT_TYPE], "application/problem+json");
        }
    }

    /// Тело больше `admin.max_body_size` — 413 problem+json: у маршрута организации и у входа по паролю, который
    /// читает тело сам.
    #[tokio::test]
    async fn body_too_large() {
        let mut cfg = config();
        cfg.max_body_size = 64;
        let env = Env::with(cfg, Vec::new());
        let big = json!({"login": "a".repeat(100), "password": "x"});
        let policy = format!("/api/v1/orgs/{}/policy", TenantId::DEFAULT);
        for (method, path, token) in [
            ("PUT", policy.as_str(), Some("root")),
            ("POST", SESSION_ROUTE, None),
        ] {
            let (s, h, b) = env.call(method, path, token, Some(big.clone())).await;
            assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{method} {path}: {b}");
            assert_eq!(
                h[header::CONTENT_TYPE],
                "application/problem+json",
                "{path}"
            );
            assert_eq!(b["status"], 413, "{path}");
        }
    }
}
