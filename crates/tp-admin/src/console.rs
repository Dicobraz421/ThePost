//! Раздача веб-консоли администратора (SRS 5.2): статика `console/dist`, вшитая в бинарный файл (`rust-embed`,
//! фича `console`), под `/console/`.
//!
//! - файл сборки — как есть; неизвестный путь — `index.html` (маршруты одностраничного приложения), кроме
//!   каталога хэшированных файлов `assets/` (там — 404);
//! - на каждом ответе — `Content-Security-Policy` без внешних источников, `X-Frame-Options: DENY`,
//!   `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`;
//! - хэшированные файлы (`assets/`) кэшируются бессрочно, остальное — `no-cache`;
//! - без фичи `console` или с пустой сборкой — страница «консоль не собрана» (503);
//! - `admin.console.enabled: false` — маршрутов нет (404).

use std::borrow::Cow;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::{Error, Result};

/// Префикс консоли; совпадает с `base` сборки (`console/vite.config.ts`).
const PREFIX: &str = "/console/";
/// Каталог файлов с хэшем содержимого в имени (Vite): имя меняется вместе с содержимым.
const HASHED: &str = "assets/";
const INDEX: &str = "index.html";

/// Секция `admin.console` config.yaml.
///
/// ```yaml
/// console:
///   enabled: true
///   connect_src: ["https://idp.example.org"]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ConsoleConfig {
    /// Раздавать веб-консоль под `/console/`; `false` — маршрутов нет (404).
    pub enabled: bool,
    /// Адреса (`https://узел[:порт]`), к которым консоли разрешены запросы помимо самого узла: поставщик OIDC
    /// (вход с PKCE). Попадают в `connect-src` политики `Content-Security-Policy`; по умолчанию — пусто.
    pub connect_src: Vec<String>,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            connect_src: Vec::new(),
        }
    }
}

impl ConsoleConfig {
    /// Проверяет `connect_src`: схема `https://` (или `http://`) и узел с необязательным портом, без пути.
    pub fn check(&self) -> Result<()> {
        for origin in &self.connect_src {
            let host = origin
                .strip_prefix("https://")
                .or_else(|| origin.strip_prefix("http://"))
                .unwrap_or_default();
            let ok = !host.is_empty()
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
            if !ok {
                return Err(Error::invalid(format!(
                    "admin: console.connect_src {origin:?} — нужен адрес вида https://узел[:порт] без пути"
                )));
            }
        }
        Ok(())
    }

    /// Значение `Content-Security-Policy`: всё — только с самого узла; стили — ещё и встроенные (Ant Design
    /// создаёт их при работе); запросы — к узлу и адресам `connect_src`.
    fn csp(&self) -> String {
        let mut connect = String::from("'self'");
        for origin in &self.connect_src {
            connect.push(' ');
            connect.push_str(origin);
        }
        format!(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             font-src 'self'; connect-src {connect}; object-src 'none'; base-uri 'self'; form-action 'self'; \
             frame-ancestors 'none'"
        )
    }
}

/// Источник файлов сборки: путь относительно `console/dist` → содержимое.
pub(crate) type Source = fn(&str) -> Option<Cow<'static, [u8]>>;

#[cfg(feature = "console")]
mod embedded {
    use std::borrow::Cow;

    /// Сборка консоли; каталога может не быть (`allow_missing`) — тогда набор пуст.
    #[derive(rust_embed::RustEmbed)]
    #[folder = "../../console/dist"]
    #[allow_missing = true]
    struct Dist;

    pub(super) fn get(path: &str) -> Option<Cow<'static, [u8]>> {
        Dist::get(path).map(|f| f.data)
    }
}

/// Вшитая сборка; без фичи `console` — `None`.
pub(crate) fn embedded() -> Option<Source> {
    #[cfg(feature = "console")]
    {
        Some(embedded::get)
    }
    #[cfg(not(feature = "console"))]
    {
        None
    }
}

struct Console {
    csp: HeaderValue,
    source: Option<Source>,
}

/// Маршруты консоли: `/` и `/console` → `/console/`, `/console/*`. Выключенная консоль — пустой набор.
pub(crate) fn router(cfg: &ConsoleConfig, source: Option<Source>) -> Result<Router> {
    if !cfg.enabled {
        return Ok(Router::new());
    }
    let csp = HeaderValue::from_str(&cfg.csp())
        .map_err(|_| Error::invalid("admin: console.connect_src — недопустимые символы"))?;
    let state = Arc::new(Console { csp, source });
    let home = || async { Redirect::temporary(PREFIX) };
    Ok(Router::new()
        .route("/", get(home))
        .route("/console", get(home))
        .route(PREFIX, get(serve))
        .route("/console/{*path}", get(serve))
        .with_state(state))
}

async fn serve(State(c): State<Arc<Console>>, uri: Uri) -> Response {
    let path = uri.path().strip_prefix(PREFIX).unwrap_or_default();
    let path = if path.is_empty() { INDEX } else { path };
    let get = |p: &str| c.source.and_then(|s| s(p));
    let mut resp = match get(path) {
        Some(data) => file(path, data),
        None if path.starts_with(HASHED) => StatusCode::NOT_FOUND.into_response(),
        None => match get(INDEX) {
            Some(data) => file(INDEX, data),
            None => not_built(),
        },
    };
    let h = resp.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, c.csp.clone());
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    resp
}

fn file(path: &str, data: Cow<'static, [u8]>) -> Response {
    let cache = if path.starts_with(HASHED) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, content_type(path)),
            (header::CACHE_CONTROL, cache),
        ],
        data,
    )
        .into_response()
}

/// Тип содержимого по расширению: только то, что бывает в сборке консоли (фича `mime-guess` не включена).
fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map_or("", |(_, e)| e);
    match ext.to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// Страница «консоль не собрана»: бинарный файл собран без фичи `console` или без `console/dist`.
fn not_built() -> Response {
    let page = format!(
        "<!doctype html>\n<html lang=\"ru\">\n<head><meta charset=\"utf-8\"><title>The Post — консоль не собрана</title></head>\n\
         <body>\n<h1>Веб-консоль не собрана</h1>\n\
         <p>Этот бинарный файл собран без веб-консоли. Как собрать консоль и вшить её в бинарный файл — \
         <a href=\"{repo}/blob/main/docs/DEV.md\">docs/DEV.md</a>.</p>\n\
         <p>Admin API и <code>thepost admin</code> работают без консоли.</p>\n</body>\n</html>\n",
        repo = env!("CARGO_PKG_REPOSITORY"),
    );
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        page,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{HeaderMap, Request};
    use tower::ServiceExt;

    use super::*;
    use crate::testenv::{Env, config};

    fn dist(path: &str) -> Option<Cow<'static, [u8]>> {
        let data: &'static [u8] = match path {
            "index.html" => b"<!doctype html><div id=root></div>",
            "assets/index-abc123.js" => b"console.log(1)",
            "favicon.svg" => b"<svg/>",
            _ => return None,
        };
        Some(Cow::Borrowed(data))
    }

    async fn fetch(
        cfg: &ConsoleConfig,
        source: Option<Source>,
        path: &str,
    ) -> (StatusCode, HeaderMap, String) {
        let req = Request::builder()
            .uri(path)
            .body(Body::empty())
            .expect("request");
        let resp = router(cfg, source)
            .expect("router")
            .oneshot(req)
            .await
            .expect("response");
        let (parts, body) = resp.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.expect("body");
        (
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    /// Приёмка ADM-15: `index.html` и файл ресурса.
    #[tokio::test]
    async fn index_and_asset() {
        let cfg = ConsoleConfig::default();
        let (s, h, b) = fetch(&cfg, Some(dist), "/console/").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(h[header::CACHE_CONTROL], "no-cache");
        assert!(b.contains("id=root"));

        let (s, h, b) = fetch(&cfg, Some(dist), "/console/assets/index-abc123.js").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "text/javascript; charset=utf-8");
        assert_eq!(
            h[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(b, "console.log(1)");

        let (s, h, _) = fetch(&cfg, Some(dist), "/console/favicon.svg").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "image/svg+xml");
        assert_eq!(h[header::CACHE_CONTROL], "no-cache");

        for path in ["/", "/console"] {
            let (s, h, _) = fetch(&cfg, Some(dist), path).await;
            assert_eq!(s, StatusCode::TEMPORARY_REDIRECT);
            assert_eq!(h[header::LOCATION], "/console/");
        }
    }

    /// Приёмка ADM-15: маршрут SPA — `index.html`; пропавший хэшированный файл — 404, а не HTML.
    #[tokio::test]
    async fn spa_route() {
        let cfg = ConsoleConfig::default();
        for path in ["/console/accounts", "/console/domains/example.org"] {
            let (s, h, b) = fetch(&cfg, Some(dist), path).await;
            assert_eq!(s, StatusCode::OK, "{path}");
            assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
            assert!(b.contains("id=root"));
        }
        let (s, h, _) = fetch(&cfg, Some(dist), "/console/assets/old-000.js").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_eq!(h[header::X_FRAME_OPTIONS], "DENY");
    }

    /// Приёмка ADM-15: заголовки безопасности; внешних источников нет, кроме заданных `connect_src`.
    #[tokio::test]
    async fn security_headers() {
        let (_, h, _) = fetch(&ConsoleConfig::default(), Some(dist), "/console/").await;
        let csp = h[header::CONTENT_SECURITY_POLICY].to_str().expect("csp");
        assert!(!csp.contains("http"), "{csp}");
        assert!(!csp.contains("unsafe-eval"), "{csp}");
        assert!(csp.contains("default-src 'self'"), "{csp}");
        assert!(csp.contains("script-src 'self';"), "{csp}");
        assert!(csp.contains("connect-src 'self';"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        assert_eq!(h[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(h[header::REFERRER_POLICY], "no-referrer");

        let cfg = ConsoleConfig {
            connect_src: vec!["https://idp.example.org".into()],
            ..ConsoleConfig::default()
        };
        let (_, h, _) = fetch(&cfg, Some(dist), "/console/assets/index-abc123.js").await;
        let csp = h[header::CONTENT_SECURITY_POLICY].to_str().expect("csp");
        assert!(
            csp.contains("connect-src 'self' https://idp.example.org;"),
            "{csp}"
        );
    }

    #[test]
    fn connect_src_is_checked() {
        let cfg = |v: &str| ConsoleConfig {
            connect_src: vec![v.into()],
            ..ConsoleConfig::default()
        };
        for good in [
            "https://idp.example.org",
            "https://idp.example.org:8443",
            "http://[::1]:8080",
        ] {
            assert!(cfg(good).check().is_ok(), "{good}");
        }
        for bad in [
            "",
            "idp.example.org",
            "https://",
            "https://idp.example.org/realms/x",
            "https://a; script-src *",
            "*",
        ] {
            assert!(cfg(bad).check().is_err(), "{bad}");
        }
        let admin = crate::AdminConfig {
            console: cfg("*"),
            ..crate::AdminConfig::default()
        };
        assert!(admin.check().is_err());
    }

    /// Без сборки (нет фичи `console` или пустой `console/dist`) — страница со ссылкой на `docs/DEV.md`.
    #[tokio::test]
    async fn not_built_page() {
        let empty: Source = |_| None;
        for source in [None, Some(empty)] {
            for path in ["/console/", "/console/accounts"] {
                let (s, h, b) = fetch(&ConsoleConfig::default(), source, path).await;
                assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
                assert!(b.contains("docs/DEV.md"), "{b}");
                assert_eq!(h[header::X_FRAME_OPTIONS], "DENY");
                assert!(h.contains_key(header::CONTENT_SECURITY_POLICY));
            }
        }
    }

    /// Консоль в составе роутера Admin API: без входа, рядом с API.
    #[tokio::test]
    async fn served_by_admin_router() {
        let env = Env::new();
        let (s, h, _) = env.call("GET", "/", None, None).await;
        assert_eq!(s, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(h[header::LOCATION], "/console/");
        let (s, h, _) = env.call("GET", "/console/accounts", None, None).await;
        assert!(
            s == StatusCode::OK || s == StatusCode::SERVICE_UNAVAILABLE,
            "{s}"
        );
        assert_eq!(h[header::X_FRAME_OPTIONS], "DENY");
        let (s, _, _) = env.call("GET", "/api/v1/whoami", None, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    /// Приёмка ADM-15: выключенная консоль — 404.
    #[tokio::test]
    async fn disabled_is_404() {
        let mut cfg = config();
        cfg.console.enabled = false;
        let env = Env::with(cfg, Vec::new());
        for path in [
            "/",
            "/console",
            "/console/",
            "/console/assets/index-abc123.js",
        ] {
            let (s, _, _) = env.call("GET", path, None, None).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{path}");
        }
        let (s, _, _) = env.call("GET", "/api/v1/whoami", Some("root"), None).await;
        assert_eq!(s, StatusCode::OK);
    }
}
