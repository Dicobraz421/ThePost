//! HTTP DAV: маршруты под `/dav/`, вход, проверка владельца, разбор методов и общие ответы.
//!
//! Вход — `Authorization: Basic` (пароль приложения) или `Bearer` (OAuth 2.0) через [`Authenticator`], как в JMAP;
//! без данных или с неверными — 401 с `WWW-Authenticate: Basic realm="dav"`. `OPTIONS` отвечает без входа
//! (клиенты узнают возможности сервера до запроса пароля). Доступ — только к своим ресурсам: логин в URL не
//! совпадает с вошедшим — 403 (общий доступ — DAV-17). Глобальная адресная книга ([`crate::gal`]) — только чтение.
//!
//! `/.well-known/caldav` и `/.well-known/carddav` перенаправляет на `/dav/` `tp-jmap` (секция `jmap.autoconfig`,
//! по умолчанию `caldav: /dav/`, `carddav: /dav/`); здесь эти пути не обслуживаются.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tp_contracts::Error;
use tp_contracts::auth::{Authenticator, Credentials, Directory, Principal};
use tp_contracts::dav::DavStore;
use tp_contracts::groups::GroupStore;
use tp_contracts::meta::AccountScope;

use crate::config::DavConfig;
use crate::path::Target;
use crate::schedule::{self, ScheduleDeps};
use crate::xml::{self, Element};
use crate::{gal, props, resource, sync};

/// Значение заголовка `DAV` (RFC 4918 §10.1, RFC 3744, RFC 4791 §5.1, RFC 6638 §2, RFC 6352 §6.1, RFC 5689).
pub const DAV_HEADER: &str =
    "1, 3, access-control, calendar-access, calendar-auto-schedule, addressbook, extended-mkcol";

/// Методы ресурсов DAV (`Allow`).
pub const ALLOW: &str =
    "OPTIONS, GET, HEAD, POST, PUT, DELETE, PROPFIND, PROPPATCH, REPORT, MKCOL, MKCALENDAR";

/// Зависимости сервера DAV: реализации контрактов, выбранные приложением.
#[derive(Clone)]
pub struct Deps {
    /// Проверка паролей приложений и Bearer-токенов.
    pub auth: Arc<dyn Authenticator>,
    /// Календари и контакты.
    pub store: Arc<dyn DavStore>,
}

/// Источники глобальной адресной книги ([`router_with_gal`]).
#[derive(Clone)]
pub struct GalDeps {
    /// Каталог организации: учётные записи и группы.
    pub directory: Arc<dyn Directory>,
    /// Группы рассылки: скрытые из адресной книги (`hidden_from_gal`) в неё не попадают; `None` — видны все
    /// группы каталога.
    pub groups: Option<Arc<dyn GroupStore>>,
}

impl fmt::Debug for GalDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GalDeps").finish_non_exhaustive()
    }
}

impl fmt::Debug for Deps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps")
            .field("limits", &self.store.limits())
            .finish_non_exhaustive()
    }
}

/// Состояние маршрутов.
#[derive(Debug)]
pub(crate) struct Dav {
    pub d: Deps,
    pub cfg: DavConfig,
    /// Источники глобальной адресной книги; `None` — книги нет.
    pub gal_deps: Option<GalDeps>,
    /// Построенные глобальные адресные книги.
    pub gal: gal::Cache,
    /// Планирование встреч; `None` — объекты хранятся без рассылки iTIP.
    pub schedule: Option<ScheduleDeps>,
}

impl Dav {
    /// Размер объекта: меньшее из конфигурации и лимита хранилища.
    pub(crate) fn max_object_size(&self) -> usize {
        self.cfg
            .max_object_size
            .min(self.d.store.limits().max_object_size)
    }
}

/// Маршруты DAV под `/dav/`; TLS, PROXY и слушатель — у вызывающего (`tp-platform`).
pub fn router(d: Deps, cfg: DavConfig) -> Router {
    build(d, None, None, cfg)
}

/// Маршруты DAV с необязательными глобальной адресной книгой `gal` и планированием встреч `schedule`
/// (RFC 6638: рассылка iTIP после `PUT` и `DELETE`, запрос занятости).
pub fn router_with(
    d: Deps,
    gal: Option<GalDeps>,
    schedule: Option<ScheduleDeps>,
    cfg: DavConfig,
) -> Router {
    build(d, gal, schedule, cfg)
}

/// Маршруты DAV с глобальной адресной книгой «Организация» из каталога `gal` (только чтение, в доме адресных
/// книг каждого пользователя).
pub fn router_with_gal(d: Deps, gal: GalDeps, cfg: DavConfig) -> Router {
    build(d, Some(gal), None, cfg)
}

fn build(
    d: Deps,
    gal_deps: Option<GalDeps>,
    schedule: Option<ScheduleDeps>,
    cfg: DavConfig,
) -> Router {
    let dav = Arc::new(Dav {
        d,
        cfg,
        gal_deps,
        gal: gal::Cache::default(),
        schedule,
    });
    Router::new()
        .route("/dav", any(handle))
        .route("/dav/", any(handle))
        .route("/dav/{*path}", any(handle))
        .with_state(dav)
}

/// Контекст запроса после входа.
pub(crate) struct Cx<'a> {
    pub dav: &'a Dav,
    pub principal: &'a Principal,
    pub scope: AccountScope,
    pub headers: &'a HeaderMap,
}

impl Cx<'_> {
    pub(crate) fn store(&self) -> &dyn DavStore {
        self.dav.d.store.as_ref()
    }

    /// Логин вошедшего (в ссылках ответов).
    pub(crate) fn login(&self) -> &str {
        &self.principal.login
    }

    /// Адреса календарного пользователя вошедшего ([`crate::itip::address`]): имя входа, если оно — адрес.
    pub(crate) fn addresses(&self) -> Vec<String> {
        let login = crate::itip::address(&self.principal.login);
        if login.contains('@') {
            vec![login]
        } else {
            Vec::new()
        }
    }

    /// Заголовок как строка без пробелов по краям.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    }

    /// XML-тело запроса: пустое — `None`; больше лимита — 413, неверное — 400.
    pub(crate) async fn xml(&self, body: Body) -> Result<Option<Element>, StatusCode> {
        let bytes = read(body, self.dav.cfg.max_request_size).await?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        match xml::parse(&bytes, self.dav.cfg.xml_limits()) {
            Ok(el) => Ok(Some(el)),
            Err(e) => {
                tracing::debug!(error = %e, "неверное XML-тело DAV");
                Err(StatusCode::BAD_REQUEST)
            }
        }
    }
}

/// Тело не больше `limit` байт; больше — 413.
pub(crate) async fn read(body: Body, limit: usize) -> Result<Bytes, StatusCode> {
    to_bytes(body, limit)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)
}

async fn handle(State(dav): State<Arc<Dav>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    if parts.method == Method::OPTIONS {
        return options();
    }
    // Адрес клиента соединения ставит сервер HTTP (`tp_platform::service_http`); без соединения — неизвестен.
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let principal = match authenticate(dav.d.auth.as_ref(), &parts.headers, peer).await {
        Ok(p) => p,
        Err(e) => return auth_error(&e),
    };
    let Some(target) = Target::parse(parts.uri.path()) else {
        return status(StatusCode::NOT_FOUND);
    };
    if let Some(login) = target.login()
        && !login.eq_ignore_ascii_case(&principal.login)
    {
        return status(StatusCode::FORBIDDEN);
    }
    let cx = Cx {
        dav: &dav,
        principal: &principal,
        scope: AccountScope {
            tenant: principal.tenant,
            account: principal.account,
        },
        headers: &parts.headers,
    };
    let method = parts.method.as_str();
    if matches!(
        method,
        "PUT" | "DELETE" | "PROPPATCH" | "MKCOL" | "MKCALENDAR"
    ) && gal::is_target(&cx, &target)
    {
        return gal::read_only(&cx, &target);
    }
    if method != "PROPFIND"
        && let Some((inbox, item)) = schedule::resource(&target)
    {
        return schedule::handle(&cx, method, inbox, item, body)
            .await
            .unwrap_or_else(|e| store_error(&e));
    }
    let result = match method {
        "PROPFIND" => props::propfind(&cx, &target, body).await,
        "PROPPATCH" => props::proppatch(&cx, &target, body).await,
        "REPORT" => sync::report(&cx, &target, body).await,
        "GET" => resource::get(&cx, &target, false).await,
        "HEAD" => resource::get(&cx, &target, true).await,
        "PUT" => resource::put(&cx, &target, body).await,
        "DELETE" => resource::delete(&cx, &target).await,
        "MKCOL" => resource::mkcol(&cx, &target, body, false).await,
        "MKCALENDAR" => resource::mkcol(&cx, &target, body, true).await,
        _ => Ok(not_allowed()),
    };
    result.unwrap_or_else(|e| store_error(&e))
}

/// Субъект по заголовку `Authorization` запроса клиента `peer`; нет заголовка или неверные данные — `Forbidden`.
async fn authenticate(
    auth: &dyn Authenticator,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
) -> tp_contracts::Result<Principal> {
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
    auth.verify(creds, peer).await
}

fn auth_error(e: &Error) -> Response {
    if e.is_retryable() {
        return status(StatusCode::SERVICE_UNAVAILABLE);
    }
    let mut resp = status(StatusCode::UNAUTHORIZED);
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"dav\", charset=\"UTF-8\""),
    );
    resp
}

fn options() -> Response {
    let mut resp = status(StatusCode::OK);
    let h = resp.headers_mut();
    h.insert("dav", HeaderValue::from_static(DAV_HEADER));
    h.insert(header::ALLOW, HeaderValue::from_static(ALLOW));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    resp
}

/// Ответ без тела.
pub(crate) fn status(code: StatusCode) -> Response {
    code.into_response()
}

/// 405 с `Allow`.
pub(crate) fn not_allowed() -> Response {
    let mut resp = status(StatusCode::METHOD_NOT_ALLOWED);
    resp.headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static(ALLOW));
    resp
}

/// XML-ответ.
pub(crate) fn xml_response(code: StatusCode, body: String) -> Response {
    (
        code,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

/// Ответ `multistatus` (207).
pub(crate) fn multistatus(ms: xml::Multistatus) -> Response {
    xml_response(StatusCode::MULTI_STATUS, ms.finish())
}

/// Ошибка с предусловием `inner` (`<d:error>`, RFC 4918 §16).
pub(crate) fn precondition(code: StatusCode, inner: &str) -> Response {
    xml_response(code, xml::error_body(inner))
}

/// Ошибка хранилища.
pub(crate) fn store_error(e: &Error) -> Response {
    let code = match e {
        Error::NotFound => StatusCode::NOT_FOUND,
        Error::Conflict => StatusCode::CONFLICT,
        Error::Invalid(msg) => {
            tracing::debug!(%msg, "DAV: недопустимые данные");
            StatusCode::BAD_REQUEST
        }
        Error::Forbidden => StatusCode::FORBIDDEN,
        Error::QuotaExceeded => StatusCode::INSUFFICIENT_STORAGE,
        Error::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
        Error::Internal(msg) => {
            tracing::error!(%msg, "DAV: внутренняя ошибка хранилища");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    status(code)
}
