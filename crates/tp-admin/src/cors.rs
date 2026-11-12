//! CORS Admin API для консоли на отдельном веб-сервере (решение 2026-11-11, п. 68; архив `console/dist`).
//!
//! Источники — `admin.console.cors_origins` ([`crate::ConsoleConfig`]); пусто — слоя нет, заголовков CORS нет.
//! Слой — `tower-http` `CorsLayer` на маршрутах `/api/v1/*` (`/console/*` и `/api/openapi.json` не затронуты):
//! ответ на preflight (`OPTIONS`) — 204, у ответов — `Access-Control-Allow-Origin`; разрешённые заголовки —
//! `Authorization`, `Content-Type`, `If-Match`; клиенту видны `Link` (страницы списков) и `ETag`.
//! `Access-Control-Allow-Credentials` не ставится: вход — заголовком `Authorization`, не cookie. Чужому источнику —
//! ответ без заголовков CORS (браузер его не пропустит); в журнал ничего не пишется.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Добавляет CORS к маршрутам `r` для источников `origins` (проверены `ConsoleConfig::check`); пусто — без изменений.
pub(crate) fn layer<S>(r: Router<S>, origins: &[String]) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let allowed: Arc<[HeaderValue]> = origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    if allowed.is_empty() {
        return r;
    }
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed.iter().cloned()))
        .allow_methods([
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::IF_MATCH,
        ])
        .expose_headers([header::LINK, header::ETAG]);
    r.layer(cors)
        .layer(middleware::from_fn_with_state(allowed, finish))
}

/// Чужой источник — без заголовков `Access-Control-*`; preflight разрешённого — 204.
async fn finish(State(allowed): State<Arc<[HeaderValue]>>, req: Request, next: Next) -> Response {
    let own = req
        .headers()
        .get(header::ORIGIN)
        .is_some_and(|o| allowed.contains(o));
    let preflight = req.method() == Method::OPTIONS
        && req
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    let mut resp = next.run(req).await;
    if !own {
        let cors: Vec<_> = resp
            .headers()
            .keys()
            .filter(|k| k.as_str().starts_with("access-control-"))
            .cloned()
            .collect();
        for k in cors {
            resp.headers_mut().remove(k);
        }
    } else if preflight && resp.status() == StatusCode::OK {
        *resp.status_mut() = StatusCode::NO_CONTENT;
    }
    resp
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode, header};
    use tower::ServiceExt;

    use crate::console::ConsoleConfig;
    use crate::server::AdminConfig;
    use crate::testenv::{Env, config};

    const CONSOLE: &str = "https://console.example.org";

    fn env(origins: &[&str]) -> Env {
        Env::with(
            AdminConfig {
                console: ConsoleConfig {
                    cors_origins: origins.iter().map(|o| (*o).to_owned()).collect(),
                    ..ConsoleConfig::default()
                },
                ..config()
            },
            Vec::new(),
        )
    }

    async fn send(env: &Env, method: &str, path: &str, origin: &str) -> (StatusCode, HeaderMap) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::ORIGIN, origin)
            .header(header::AUTHORIZATION, "Bearer root");
        if method == "OPTIONS" {
            req = req
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "PUT")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "authorization, if-match",
                );
        }
        let resp = env
            .router
            .clone()
            .oneshot(req.body(Body::empty()).expect("request"))
            .await
            .expect("response");
        (resp.status(), resp.headers().clone())
    }

    fn has_cors(h: &HeaderMap) -> bool {
        h.keys().any(|k| k.as_str().starts_with("access-control-"))
    }

    #[tokio::test]
    async fn preflight_from_allowed_origin() {
        let env = env(&[CONSOLE]);
        let (s, h) = send(&env, "OPTIONS", "/api/v1/whoami", CONSOLE).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], CONSOLE);
        let allowed = h[header::ACCESS_CONTROL_ALLOW_HEADERS]
            .to_str()
            .expect("headers")
            .to_ascii_lowercase();
        for name in ["authorization", "content-type", "if-match"] {
            assert!(allowed.contains(name), "{allowed}");
        }
        assert!(h.get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none());
        let (s, h) = send(&env, "GET", "/api/v1/whoami", CONSOLE).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], CONSOLE);
        let (_, h) = send(&env, "GET", "/api/v1/session", CONSOLE).await;
        assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], CONSOLE);
    }

    #[tokio::test]
    async fn foreign_origin_without_headers() {
        let env = env(&[CONSOLE]);
        let (_, h) = send(&env, "OPTIONS", "/api/v1/whoami", "https://evil.example").await;
        assert!(!has_cors(&h), "{h:?}");
        let (s, h) = send(&env, "GET", "/api/v1/whoami", "https://evil.example").await;
        assert_eq!(s, StatusCode::OK);
        assert!(!has_cors(&h), "{h:?}");
    }

    #[tokio::test]
    async fn empty_setting_and_console_untouched() {
        let env = env(&[]);
        let (_, h) = send(&env, "OPTIONS", "/api/v1/whoami", CONSOLE).await;
        assert!(!has_cors(&h), "{h:?}");
        let (_, h) = send(&env, "GET", "/api/v1/whoami", CONSOLE).await;
        assert!(!has_cors(&h), "{h:?}");

        let env = self::env(&[CONSOLE]);
        let (_, h) = send(&env, "GET", "/console/", CONSOLE).await;
        assert!(!has_cors(&h), "{h:?}");
    }

    #[test]
    fn origins_checked() {
        let bad = ConsoleConfig {
            cors_origins: vec!["http://console.example.org".into()],
            ..ConsoleConfig::default()
        };
        assert!(bad.check().is_err());
        let ok = ConsoleConfig {
            cors_origins: vec!["https://console.example.org:8443".into()],
            ..ConsoleConfig::default()
        };
        assert!(ok.check().is_ok());
    }
}
