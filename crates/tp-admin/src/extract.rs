//! Разбор запроса для всех маршрутов Admin API: `Json`, `Query`, `Path` поверх одноимённых извлекателей `axum` с
//! одним отказом — 400 `application/problem+json`, в `detail` — поле и причина (путь до поля — `serde_path_to_error`
//! внутри `axum`). Слишком большое тело — код `axum` (413), тоже problem+json. `Json` — и ответ: тело JSON.

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

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for Json<T> {
    type Rejection = Refused;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Self(v)),
            Err(JsonRejection::JsonDataError(e)) => Err(invalid("тело запроса", cause(&e))),
            Err(JsonRejection::JsonSyntaxError(e)) => {
                Err(invalid("тело запроса — не JSON", cause(&e)))
            }
            Err(JsonRejection::MissingJsonContentType(_)) => Err(invalid(
                "тело запроса",
                "нужен заголовок Content-Type: application/json".into(),
            )),
            Err(JsonRejection::BytesRejection(e)) if e.status() != StatusCode::BAD_REQUEST => {
                Err(Refused::Problem(e.status(), e.body_text()))
            }
            Err(e) => Err(invalid("тело запроса", e.body_text())),
        }
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
    use tp_contracts::TenantId;

    use crate::testenv::Env;

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
}
