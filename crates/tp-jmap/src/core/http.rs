//! HTTP JMAP (RFC 8620 §2, §3.1): `/.well-known/jmap` и `/jmap/session` — объект сессии,
//! `POST /jmap/api` — запрос API. Аутентификация — `Authorization: Bearer` (OAuth 2.0) или
//! `Basic` (пароль приложения) через [`Authenticator`](tp_contracts::auth::Authenticator).
//! Загрузка и скачивание blob — `crate::blob`, push (EventSource, WebSocket) — `crate::push`.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use tp_contracts::Error;
use tp_contracts::auth::{Credentials, Principal};

use super::{Jmap, Problem};

impl Jmap {
    /// Маршруты JMAP; TLS, PROXY и слушатель — у вызывающего (`tp-platform`).
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/.well-known/jmap", get(session))
            .route("/jmap/session", get(session))
            .route("/jmap/api", post(api))
            .route("/jmap/upload/{account}/", post(crate::blob::upload))
            .route(
                "/jmap/download/{account}/{blob}/{name}",
                get(crate::blob::download),
            )
            .route("/jmap/eventsource/", get(crate::push::eventsource))
            .route("/jmap/ws", get(crate::push::websocket))
            .with_state(self)
    }

    /// Субъект по заголовку `Authorization`; нет заголовка или неверные данные — `Forbidden`.
    pub async fn authenticate(&self, headers: &HeaderMap) -> tp_contracts::Result<Principal> {
        let value = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(Error::Forbidden)?;
        let (scheme, rest) = value.split_once(' ').ok_or(Error::Forbidden)?;
        let rest = rest.trim();
        let creds = if scheme.eq_ignore_ascii_case("bearer") {
            Credentials::Bearer(rest.to_owned())
        } else if scheme.eq_ignore_ascii_case("basic") {
            let raw = STANDARD.decode(rest).map_err(|_| Error::Forbidden)?;
            let text = String::from_utf8(raw).map_err(|_| Error::Forbidden)?;
            let (login, secret) = text.split_once(':').ok_or(Error::Forbidden)?;
            Credentials::Password {
                login: login.to_owned(),
                secret: secret.to_owned(),
            }
        } else {
            return Err(Error::Forbidden);
        };
        self.d.auth.verify(creds).await
    }

    pub(crate) fn base_url(&self, headers: &HeaderMap) -> String {
        match &self.cfg.public_url {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => {
                let host = headers
                    .get(header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("localhost");
                format!("https://{host}")
            }
        }
    }
}

pub(crate) fn json(status: StatusCode, content_type: &'static str, body: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
        body.to_string(),
    )
        .into_response()
}

pub(crate) fn problem(p: &Problem) -> Response {
    let status = StatusCode::from_u16(p.status).unwrap_or(StatusCode::BAD_REQUEST);
    json(status, "application/problem+json", &p.to_json())
}

pub(crate) fn auth_error(e: &Error) -> Response {
    if e.is_retryable() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let mut resp = StatusCode::UNAUTHORIZED.into_response();
    let h = resp.headers_mut();
    h.append(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"jmap\", charset=\"UTF-8\""),
    );
    h.append(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"jmap\""),
    );
    resp
}

async fn session(State(j): State<Arc<Jmap>>, headers: HeaderMap) -> Response {
    let p = match j.authenticate(&headers).await {
        Ok(p) => p,
        Err(e) => return auth_error(&e),
    };
    let s = j.session(&p, &j.base_url(&headers));
    let mut resp = json(StatusCode::OK, "application/json", &s);
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-store"),
    );
    resp
}

async fn api(State(j): State<Arc<Jmap>>, headers: HeaderMap, body: Body) -> Response {
    let p = match j.authenticate(&headers).await {
        Ok(p) => p,
        Err(e) => return auth_error(&e),
    };
    let limit = usize::try_from(j.cfg.max_size_request).unwrap_or(usize::MAX);
    let Ok(body) = to_bytes(body, limit).await else {
        return problem(&Problem::limit("maxSizeRequest"));
    };
    match j.api(&p, &body).await {
        Ok(r) => match serde_json::to_value(&r) {
            Ok(v) => json(StatusCode::OK, "application/json", &v),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(p) => problem(&p),
    }
}
