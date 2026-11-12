//! Веб-консоль администратора (SRS 5.2): статика `console/dist`, вшитая в бинарный файл (`rust-embed`, фича `console`).
//!
//! `GET /console/*` — файл сборки; неизвестный путь вне `assets/` — `index.html` (маршруты SPA); `/` → `/console/`.
//! Ответы несут `Content-Security-Policy` без внешних источников и `X-Frame-Options: DENY`; файлы `assets/`
//! (имя с хэшем содержимого) кэшируются надолго, остальное — с перепроверкой. Без фичи `console` или без сборки
//! консоли — страница «консоль не собрана». `admin.console.enabled: false` — маршрутов нет (404).

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::config::duration;
use tp_contracts::{Error, Result};

/// Адрес консоли; совпадает с `base` сборки (`console/vite.config.ts`).
const CONSOLE: &str = "/console/";

/// Политика содержимого: только свой источник. Встроенные стили — для antd (CSS-in-JS), `data:` — значки и шрифты.
const CSP_HEAD: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                        img-src 'self' data:; font-src 'self' data:; connect-src 'self'";
const CSP_TAIL: &str =
    "; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";

/// Страница вместо консоли, когда её сборки в бинарном файле нет.
const NOT_BUILT: &str = concat!(
    "<!doctype html>\n<html lang=\"ru\">\n<head><meta charset=\"UTF-8\">",
    "<title>The Post — консоль не собрана</title></head>\n<body>\n<h1>Консоль не собрана</h1>\n",
    "<p>Этот бинарный файл собран без веб-консоли администратора. Admin API и <code>thepost admin</code> работают.</p>\n",
    "<p>Как собрать консоль — <a href=\"",
    env!("CARGO_PKG_REPOSITORY"),
    "/blob/main/docs/DEV.md\">docs/DEV.md</a>.</p>\n</body>\n</html>\n"
);

/// Секция `admin.console`: раздача веб-консоли ролью admin.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ConsoleConfig {
    /// Раздавать консоль; `false` — `/` и `/console/*` отвечают 404.
    pub enabled: bool,
    /// Срок кэширования файлов с хэшем в имени (`assets/`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub asset_max_age: Duration,
    /// Дополнительные источники `connect-src` (поставщик OIDC, например `https://idp.example.org`); по умолчанию — нет.
    pub connect_src: Vec<String>,
    /// Источники консоли на отдельном веб-сервере (`https://узел[:порт]`, архив `console/dist`): им Admin API
    /// отвечает с заголовками CORS ([`crate::cors`]); по умолчанию — нет, заголовков CORS нет.
    pub cors_origins: Vec<String>,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            asset_max_age: Duration::from_secs(365 * 24 * 3600),
            connect_src: Vec::new(),
            cors_origins: Vec::new(),
        }
    }
}

impl ConsoleConfig {
    /// Проверяет значения: источники `connect_src` и `cors_origins` — `https://узел[:порт]` без пути и служебных знаков.
    pub fn check(&self) -> Result<()> {
        for (field, list) in [
            ("connect_src", &self.connect_src),
            ("cors_origins", &self.cors_origins),
        ] {
            for src in list {
                let host = src.strip_prefix("https://").unwrap_or_default();
                let ok = !host.is_empty()
                    && host.chars().all(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')
                    });
                if !ok {
                    return Err(Error::invalid(format!(
                        "admin.console: {field} {src:?} — ожидается https://узел[:порт]"
                    )));
                }
            }
        }
        Ok(())
    }

    fn csp(&self) -> HeaderValue {
        let mut csp = String::from(CSP_HEAD);
        for src in &self.connect_src {
            csp.push(' ');
            csp.push_str(src);
        }
        csp.push_str(CSP_TAIL);
        // Значения проверены `check`; непроверенная конфигурация — политика без дополнительных источников.
        HeaderValue::from_str(&csp).unwrap_or_else(|_| {
            HeaderValue::from_static("default-src 'self'; frame-ancestors 'none'")
        })
    }
}

/// Источник файлов сборки по пути относительно `console/dist`.
pub(crate) type Lookup = fn(&str) -> Option<Cow<'static, [u8]>>;

#[cfg(feature = "console")]
mod embedded {
    use std::borrow::Cow;

    /// Сборка консоли; каталога может не быть — тогда файлов нет.
    #[derive(rust_embed::RustEmbed)]
    #[folder = "../../console/dist"]
    #[allow_missing = true]
    struct Dist;

    pub(super) fn lookup(path: &str) -> Option<Cow<'static, [u8]>> {
        <Dist as rust_embed::RustEmbed>::get(path).map(|f| f.data)
    }
}

/// Вшитая сборка консоли; без фичи `console` файлов нет.
pub(crate) fn embedded() -> Lookup {
    #[cfg(feature = "console")]
    {
        embedded::lookup
    }
    #[cfg(not(feature = "console"))]
    {
        |_| None
    }
}

struct Site {
    lookup: Lookup,
    csp: HeaderValue,
    asset_cache: HeaderValue,
}

enum Found {
    File {
        mime: &'static str,
        hashed: bool,
        body: Cow<'static, [u8]>,
    },
    NotBuilt,
    Missing,
}

/// Тип содержимого по расширению: только то, что бывает в сборке консоли.
fn mime(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("txt") => "text/plain; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn resolve(lookup: Lookup, path: &str) -> Found {
    if path.split('/').any(|seg| seg == "..") {
        return Found::Missing;
    }
    if !path.is_empty() {
        if let Some(body) = lookup(path) {
            return Found::File {
                mime: mime(path),
                hashed: path.starts_with("assets/"),
                body,
            };
        }
        // Пропавший файл `assets/` не подменяется страницей. По расширению судить нельзя: в маршрутах SPA
        // бывает точка (`domains/example.org`).
        if path.starts_with("assets/") {
            return Found::Missing;
        }
    }
    match lookup("index.html") {
        Some(body) => Found::File {
            mime: "text/html; charset=utf-8",
            hashed: false,
            body,
        },
        None => Found::NotBuilt,
    }
}

async fn serve(site: Arc<Site>, path: String) -> Response {
    let lookup = site.lookup;
    // В отладочной сборке `rust-embed` читает файлы с диска — вне потоков async.
    let found = tokio::task::spawn_blocking(move || resolve(lookup, &path)).await;
    let html = HeaderValue::from_static("text/html; charset=utf-8");
    let no_cache = HeaderValue::from_static("no-cache");
    let mut resp = match found {
        Ok(Found::File { mime, hashed, body }) => {
            let cache = if hashed {
                site.asset_cache.clone()
            } else {
                no_cache
            };
            let mut r = body.into_owned().into_response();
            r.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
            r.headers_mut().insert(header::CACHE_CONTROL, cache);
            r
        }
        Ok(Found::NotBuilt) => {
            let mut r = (StatusCode::SERVICE_UNAVAILABLE, NOT_BUILT).into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, html);
            r.headers_mut().insert(header::CACHE_CONTROL, no_cache);
            r
        }
        Ok(Found::Missing) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let h = resp.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, site.csp.clone());
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

/// Добавляет маршруты консоли (без входа: статика открыта, данные — только через Admin API).
pub(crate) fn routes<S>(r: Router<S>, cfg: &ConsoleConfig, lookup: Lookup) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    if !cfg.enabled {
        return r;
    }
    let max_age = format!("public, max-age={}, immutable", cfg.asset_max_age.as_secs());
    let site = Arc::new(Site {
        lookup,
        csp: cfg.csp(),
        asset_cache: HeaderValue::from_str(&max_age)
            .unwrap_or_else(|_| HeaderValue::from_static("no-cache")),
    });
    let index = site.clone();
    r.route("/", get(|| async { Redirect::temporary(CONSOLE) }))
        .route("/console", get(|| async { Redirect::temporary(CONSOLE) }))
        .route(CONSOLE, get(move || serve(index, String::new())))
        .route(
            "/console/{*path}",
            get(move |Path(path): Path<String>| serve(site, path)),
        )
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;

    use super::*;

    fn dist(path: &str) -> Option<Cow<'static, [u8]>> {
        match path {
            "index.html" => Some(Cow::Borrowed(b"<!doctype html><div id=root>")),
            "assets/index-C0ffee12.js" => Some(Cow::Borrowed(b"console.log(1)")),
            "favicon.svg" => Some(Cow::Borrowed(b"<svg/>")),
            _ => None,
        }
    }

    fn empty(_: &str) -> Option<Cow<'static, [u8]>> {
        None
    }

    async fn call(
        cfg: &ConsoleConfig,
        lookup: Lookup,
        uri: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let router: Router = routes(Router::new(), cfg, lookup);
        let req = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request");
        let resp = tower::ServiceExt::oneshot(router, req)
            .await
            .expect("response");
        let (parts, body) = resp.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.expect("body");
        (parts.status, parts.headers, body.to_vec())
    }

    #[tokio::test]
    async fn index_and_asset() {
        let cfg = ConsoleConfig::default();
        let (s, h, b) = call(&cfg, dist, "/console/").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(h[header::CACHE_CONTROL], "no-cache");
        assert!(b.starts_with(b"<!doctype html>"));

        let (s, h, b) = call(&cfg, dist, "/console/assets/index-C0ffee12.js").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "text/javascript; charset=utf-8");
        assert_eq!(
            h[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(b, b"console.log(1)");

        let (s, h, _) = call(&cfg, dist, "/console/favicon.svg").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "image/svg+xml");
        assert_eq!(h[header::CACHE_CONTROL], "no-cache");
    }

    #[tokio::test]
    async fn spa_route_and_missing_asset() {
        let cfg = ConsoleConfig::default();
        let (s, h, b) = call(&cfg, dist, "/console/orgs/example.org/accounts").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert!(b.starts_with(b"<!doctype html>"));

        // Маршрут с точкой в последнем сегменте (карточка учётной записи) — тоже страница, а не 404.
        let (s, h, b) = call(&cfg, dist, "/console/accounts/user@example.org").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert!(b.starts_with(b"<!doctype html>"));

        let (s, _, _) = call(&cfg, dist, "/console/assets/gone-12345678.js").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = call(&cfg, dist, "/console/../index.html").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn redirects_to_console() {
        let cfg = ConsoleConfig::default();
        for uri in ["/", "/console"] {
            let (s, h, _) = call(&cfg, dist, uri).await;
            assert_eq!(s, StatusCode::TEMPORARY_REDIRECT);
            assert_eq!(h[header::LOCATION], "/console/");
        }
    }

    #[tokio::test]
    async fn security_headers() {
        let cfg = ConsoleConfig {
            connect_src: vec!["https://idp.example.org".into()],
            ..ConsoleConfig::default()
        };
        cfg.check().expect("config");
        for (lookup, uri) in [
            (dist as Lookup, "/console/"),
            (dist, "/console/assets/index-C0ffee12.js"),
            (empty, "/console/"),
        ] {
            let (_, h, _) = call(&cfg, lookup, uri).await;
            let csp = h[header::CONTENT_SECURITY_POLICY].to_str().expect("csp");
            assert!(csp.starts_with("default-src 'self'; script-src 'self';"));
            assert!(csp.contains("connect-src 'self' https://idp.example.org;"));
            assert!(csp.contains("frame-ancestors 'none'"));
            assert!(!csp.contains("http://") && !csp.contains('*'));
            assert_eq!(h[header::X_FRAME_OPTIONS], "DENY");
            assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        }
        let (_, h, _) = call(&ConsoleConfig::default(), dist, "/console/").await;
        let csp = h[header::CONTENT_SECURITY_POLICY].to_str().expect("csp");
        assert!(!csp.contains("https://"));
    }

    #[tokio::test]
    async fn not_built_page() {
        let (s, h, b) = call(&ConsoleConfig::default(), empty, "/console/accounts").await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
        let text = String::from_utf8(b).expect("utf-8");
        assert!(text.contains("Консоль не собрана"));
        assert!(text.contains("/docs/DEV.md"));
    }

    #[tokio::test]
    async fn disabled_is_404() {
        let cfg = ConsoleConfig {
            enabled: false,
            ..ConsoleConfig::default()
        };
        for uri in [
            "/",
            "/console",
            "/console/",
            "/console/assets/index-C0ffee12.js",
        ] {
            let (s, _, _) = call(&cfg, dist, uri).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[test]
    fn config_check() {
        let bad = |src: &str| ConsoleConfig {
            connect_src: vec![src.into()],
            ..ConsoleConfig::default()
        };
        assert!(bad("https://idp.example.org:8443").check().is_ok());
        for src in [
            "http://idp.example.org",
            "https://",
            "https://a b",
            "https://a; script-src *",
            "*",
        ] {
            assert!(bad(src).check().is_err(), "{src}");
        }
        let cfg: ConsoleConfig =
            serde_json::from_value(serde_json::json!({ "enabled": false, "asset_max_age": "1h" }))
                .expect("config");
        assert!(!cfg.enabled);
        assert_eq!(cfg.asset_max_age, Duration::from_secs(3600));
    }
}
