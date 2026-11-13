//! Клиент Admin API (решение 118, ADM-44): общий для CLI `thepost admin` и текстового интерфейса. Своей логики
//! управления у клиента нет — только вход, запросы и разбор ответов.
//!
//! - Вход — `Authorization: Bearer`: сессия Admin API (`tps_…`, [`Client::login`] — вход по паролю,
//!   `POST /api/v1/session`, решение 117) или токен OIDC ([`oidc::DeviceLogin`] — вход с устройства, RFC 8628).
//!   Одноразовый токен `admin init` (`tpa_…`) клиент не принимает ([`token`]).
//! - Отказ Admin API — [`Error::Api`]: код и сообщение из problem details (RFC 9457).
//! - Листание — по ссылке `Link: <…>; rel="next"` (RFC 8288, [`Client::call_page`], [`Client::pages`]); у журналов
//!   курсор `next` — поле ответа tp-admin.
//!
//! Тела запросов, параметры и ответы — прямо структуры tp-admin, из которых строится спецификация OpenAPI
//! (APP-21, APP-22; у них обе стороны serde — ADM-13): своих копий типов у клиента нет. Операции — [`ops`]: тест
//! сверяет каждую со спецификацией. Генератор клиента (progenitor) не используется: спецификация — OpenAPI 3.1,
//! progenitor читает только 3.0.

pub mod oidc;
pub mod ops;

use std::path::PathBuf;
use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, HeaderMap, LINK};
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tp_admin::init::{AdminSession, PasswordLogin};
use tp_admin::server::{Problem, WhoAmI};
use tp_admin::sieve::ScriptProblem;
use tp_contracts::TenantId;
use tp_contracts::admin_token::is_local_admin_token;

use crate::ops::{Op, SESSION, WHOAMI};

/// Переменная окружения: адрес Admin API.
pub const ENV_URL: &str = "THEPOST_ADMIN_URL";
/// Переменная окружения: токен (сессия `tps_…` или токен OIDC).
pub const ENV_TOKEN: &str = "THEPOST_ADMIN_TOKEN";
/// Переменная окружения: организация (UUID).
pub const ENV_ORG: &str = "THEPOST_ADMIN_ORG";

/// Предельное время запроса по умолчанию, секунд.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Запрос без тела.
pub const NO_BODY: Option<&()> = None;

/// Ошибка клиента; текст — для человека.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Отказ Admin API (ответ не 2xx): код, problem details (RFC 9457), если разобраны, и сообщение с подсказкой.
    #[error("{message}")]
    Api {
        /// Код HTTP.
        status: u16,
        /// Тело отказа.
        problem: Option<Problem>,
        /// Сообщение: операция, код, описание и подсказка.
        message: String,
    },
    /// Прочее: адрес, настройки, сеть, ответ не по спецификации, поставщик OIDC.
    #[error("{0}")]
    Client(String),
}

impl From<String> for Error {
    fn from(msg: String) -> Self {
        Self::Client(msg)
    }
}

impl From<Error> for String {
    fn from(e: Error) -> Self {
        e.to_string()
    }
}

/// Результат клиента.
pub type Result<T> = std::result::Result<T, Error>;

/// Канал к Admin API.
#[derive(Debug, Clone)]
pub struct Options {
    /// Корневые сертификаты (PEM) для проверки TLS Admin API.
    pub ca_file: Option<PathBuf>,
    /// Не проверять сертификат TLS — только для loopback узла с сертификатом на другое имя.
    pub insecure: bool,
    /// Предельное время запроса.
    pub timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            ca_file: None,
            insecure: false,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }
}

/// Значение настройки: `explicit`, иначе переменная окружения `var`; без пробелов по краям, пустое — нет.
pub fn setting(explicit: Option<String>, var: &str) -> Option<String> {
    explicit
        .or_else(|| std::env::var(var).ok())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Токен входа: `explicit` или `THEPOST_ADMIN_TOKEN` — сессия или токен OIDC, не одноразовый токен `admin init`.
pub fn token(explicit: Option<String>) -> Result<String> {
    let token = setting(explicit, ENV_TOKEN).ok_or_else(|| {
        format!(
            "нет токена: задайте {ENV_TOKEN} (сессия tps_… из thepost admin login или токен OIDC) или --token"
        )
    })?;
    if is_local_admin_token(&token) {
        return Err(Error::Client(
            "одноразовый токен admin init (tpa_…) не принимается: войдите по паролю — \
             thepost admin login --user <адрес>"
                .to_owned(),
        ));
    }
    Ok(token)
}

/// Адрес Admin API из строки `raw`: только схемы `http` и `https`.
pub fn base_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).map_err(|e| format!("адрес Admin API {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Client(format!(
            "адрес Admin API {raw:?}: нужна схема http или https"
        )));
    }
    Ok(url)
}

/// Параметры запроса из структуры запроса tp-admin: поля объекта JSON, `null` пропускается.
pub fn query<Q: Serialize>(q: &Q) -> Result<Vec<(String, String)>> {
    match serde_json::to_value(q).map_err(|e| e.to_string())? {
        serde_json::Value::Object(fields) => Ok(fields
            .into_iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::Null => None,
                serde_json::Value::String(s) => Some((k, s)),
                other => Some((k, other.to_string())),
            })
            .collect()),
        other => Err(Error::Client(format!(
            "параметры запроса — не объект: {other}"
        ))),
    }
}

/// Клиент Admin API: адрес и токен входа (нет — только вход по паролю).
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    token: Option<String>,
}

impl Client {
    /// Клиент к `base` с токеном `token`; канал — `options`.
    pub fn new(base: Url, token: String, options: &Options) -> Result<Self> {
        Self::build(base, Some(token), options)
    }

    /// Клиент без токена — для входа по паролю ([`Client::login`]).
    pub fn anonymous(base: Url, options: &Options) -> Result<Self> {
        Self::build(base, None, options)
    }

    fn build(base: Url, token: Option<String>, options: &Options) -> Result<Self> {
        let mut b = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(options.timeout.max(Duration::from_secs(1)));
        if let Some(path) = &options.ca_file {
            let pem = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(|e| format!("{}: не PEM: {e}", path.display()))?;
            for c in certs {
                b = b.add_root_certificate(c);
            }
        }
        if options.insecure {
            b = b.danger_accept_invalid_certs(true);
        }
        let http = b.build().map_err(|e| format!("клиент HTTP: {e}"))?;
        Ok(Self { http, base, token })
    }

    /// Адрес операции: параметры шаблона по порядку (кодируются как сегменты пути) и запрос `query`.
    fn url(&self, op: Op, params: &[&str], query: &[(String, String)]) -> Result<Url> {
        let mut url = self.base.clone();
        {
            let mut segs = url
                .path_segments_mut()
                .map_err(|()| format!("адрес Admin API {}: не HTTP", self.base))?;
            segs.pop_if_empty();
            let mut params = params.iter();
            for seg in op.path.split('/').filter(|s| !s.is_empty()) {
                if seg.starts_with('{') {
                    let value = params
                        .next()
                        .ok_or_else(|| format!("{}: нет параметра {seg}", op.path))?;
                    segs.push(value);
                } else {
                    segs.push(seg);
                }
            }
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    /// Запрос `op` с телом `body` (JSON); ответ 2xx — заголовки и тело как есть, иначе — [`Error::Api`].
    async fn send<B: Serialize + ?Sized>(
        &self,
        op: Op,
        params: &[&str],
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<(HeaderMap, Vec<u8>)> {
        let url = self.url(op, params, query)?;
        let method = Method::from_bytes(op.method.as_bytes()).map_err(|e| e.to_string())?;
        let mut req = self.http.request(method, url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        if let Some(body) = body {
            let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
            req = req.header(CONTENT_TYPE, "application/json").body(bytes);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("Admin API {}: {e}", self.base))?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("Admin API: ответ: {e}"))?;
        if status.is_success() {
            Ok((headers, bytes.to_vec()))
        } else {
            Err(problem(op, status, &bytes))
        }
    }

    /// Запрос `op`; ответ — структура ответа tp-admin `R`.
    pub async fn call<B: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        op: Op,
        params: &[&str],
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<R> {
        self.call_page(op, params, query, body)
            .await
            .map(|(r, _)| r)
    }

    /// Запрос `op`; ответ — структура ответа tp-admin `R` и цель ссылки `Link: <…>; rel="next"` (RFC 8288), если есть.
    pub async fn call_page<B: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        op: Op,
        params: &[&str],
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<(R, Option<String>)> {
        let (headers, bytes) = self.send(op, params, query, body).await?;
        let r = serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "{} {}: ответ не по спецификации Admin API: {e}",
                op.method, op.path
            )
        })?;
        Ok((r, next_link(&headers)))
    }

    /// Параметры запроса из ссылки Admin API (`/api/v1/…?…`, путь от корня Admin API).
    pub fn link_query(&self, link: &str) -> Result<Vec<(String, String)>> {
        let url = self
            .base
            .join(link)
            .map_err(|e| format!("ссылка Link {link:?}: {e}"))?;
        Ok(url.query_pairs().into_owned().collect())
    }

    /// Список `op` (`GET` без тела) страницами по ссылке `rel="next"`, не больше `limit` записей (`None` — все);
    /// второе значение — остались записи сверх `limit`.
    pub async fn pages<R: DeserializeOwned>(
        &self,
        op: Op,
        params: &[&str],
        query: Vec<(String, String)>,
        limit: Option<usize>,
    ) -> Result<(Vec<R>, bool)> {
        let mut query = query;
        let mut out = Vec::new();
        loop {
            let (page, next): (Vec<R>, _) = self.call_page(op, params, &query, NO_BODY).await?;
            let empty = page.is_empty();
            out.extend(page);
            if let Some(limit) = limit.filter(|l| out.len() >= *l) {
                let more = out.len() > limit || next.is_some();
                out.truncate(limit);
                return Ok((out, more));
            }
            match next {
                // Пустая страница со ссылкой дальше — ошибка узла; без проверки — бесконечный цикл.
                Some(_) if empty => {
                    return Err(Error::Client(format!(
                        "{} {}: пустая страница со ссылкой rel=\"next\"",
                        op.method, op.path
                    )));
                }
                Some(link) => query = self.link_query(&link)?,
                None => return Ok((out, false)),
            }
        }
    }

    /// Запрос `op` без тела ответа (`204`).
    pub async fn call_empty<B: Serialize + ?Sized>(
        &self,
        op: Op,
        params: &[&str],
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<()> {
        self.send(op, params, query, body).await.map(drop)
    }

    /// Вход администратора по паролю (`POST /api/v1/session`): сессия Admin API.
    pub async fn login(&self, user: &str, password: String) -> Result<AdminSession> {
        let body = PasswordLogin {
            login: user.trim().to_owned(),
            password,
        };
        self.call(SESSION, &[], &[], Some(&body)).await
    }

    /// Текущий администратор и его роли.
    pub async fn whoami(&self) -> Result<WhoAmI> {
        self.call(WHOAMI, &[], &[], NO_BODY).await
    }

    /// Организация запросов: `explicit`, иначе `THEPOST_ADMIN_ORG`, иначе организация администратора (whoami).
    pub async fn org(&self, explicit: Option<String>) -> Result<String> {
        let org = match setting(explicit, ENV_ORG) {
            Some(o) => o,
            None => self.whoami().await?.tenant,
        };
        org.parse::<TenantId>()
            .map_err(|e| format!("организация {org:?}: {e}"))?;
        Ok(org)
    }
}

/// Цель первой ссылки `rel="next"` заголовков `Link` (RFC 8288).
fn next_link(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(LINK)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .find_map(|link| {
            let (target, params) = link.trim().strip_prefix('<')?.split_once('>')?;
            params
                .split(';')
                .filter_map(|p| p.trim().strip_prefix("rel="))
                .any(|rel| {
                    rel.trim_matches('"')
                        .split_ascii_whitespace()
                        .any(|r| r.eq_ignore_ascii_case("next"))
                })
                .then(|| target.to_owned())
        })
}

/// Отказ: код, описание и подробность ответа (RFC 9457, [`Problem`]), ошибки текста скрипта, подсказка для входа.
fn problem(op: Op, status: StatusCode, body: &[u8]) -> Error {
    let p = serde_json::from_slice::<Problem>(body).ok();
    let title = p
        .as_ref()
        .map(|p| p.title.as_str())
        .filter(|t| !t.is_empty())
        .or_else(|| status.canonical_reason())
        .unwrap_or_default();
    let mut message = format!("{} {}: {} {title}", op.method, op.path, status.as_u16());
    if let Some(detail) = p.as_ref().and_then(|p| p.detail.as_deref()) {
        message.push_str(&format!(": {detail}"));
    }
    if let Ok(script) = serde_json::from_slice::<ScriptProblem>(body) {
        for e in &script.errors {
            message.push_str(&format!(
                "\n  строка {}, столбец {}: {}",
                e.line, e.column, e.message
            ));
        }
    }
    let hint = match (op == SESSION, status) {
        (true, StatusCode::UNAUTHORIZED) => {
            " (неверное имя входа или пароль, либо вход временно заблокирован после неудачных попыток)"
        }
        (true, StatusCode::FORBIDDEN) => {
            " (у учётной записи нет ролей администрирования: admin.roles)"
        }
        (true, StatusCode::NOT_FOUND) => {
            " (вход по паролю выключен: admin.password_login; вход через OIDC — thepost admin login --oidc)"
        }
        (false, StatusCode::UNAUTHORIZED) => {
            " (токен не принят: сессия истекла или токен OIDC неверен; новая сессия — thepost admin login)"
        }
        _ => "",
    };
    message.push_str(hint);
    Error::Api {
        status: status.as_u16(),
        problem: p,
        message,
    }
}

#[cfg(test)]
mod tests {
    use tp_admin::accounts::AccountQuery;
    use tp_admin::transport::VersionQuery;

    use super::*;

    #[test]
    fn token_kinds() {
        let err = token(Some("tpa_x".to_owned())).expect_err("local token");
        assert!(
            err.to_string().contains("thepost admin login --user"),
            "{err}"
        );
        assert_eq!(token(Some(" tps_x ".to_owned())).expect("session"), "tps_x");
    }

    #[test]
    fn base_url_schemes() {
        assert_eq!(
            base_url("http://127.0.0.1:9443").expect("url").as_str(),
            "http://127.0.0.1:9443/"
        );
        let err = base_url("ftp://127.0.0.1").expect_err("ftp");
        assert!(err.to_string().contains("http или https"), "{err}");
    }

    /// Параметры запроса — из структур запроса tp-admin; `null` пропускается.
    #[test]
    fn query_from_tp_admin_structs() {
        let sorted = |mut v: Vec<(String, String)>| {
            v.sort();
            v
        };
        let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        assert_eq!(
            query(&AccountQuery {
                login: Some("a@b.test".into()),
                ..AccountQuery::default()
            }),
            Ok(vec![pair("login", "a@b.test")])
        );
        assert_eq!(
            query(&AccountQuery {
                login: None,
                prefix: Some("a".into()),
                after: Some("a@b.test".into()),
                limit: Some(10),
            })
            .map(sorted),
            Ok(vec![
                pair("after", "a@b.test"),
                pair("limit", "10"),
                pair("prefix", "a")
            ])
        );
        assert_eq!(
            query(&VersionQuery { version: 3 }),
            Ok(vec![pair("version", "3")])
        );
    }

    /// Ссылка `rel="next"` (RFC 8288): среди нескольких ссылок, `rel` с несколькими значениями, без кавычек.
    #[test]
    fn next_link_header() {
        let headers = |values: &[&str]| {
            let mut h = HeaderMap::new();
            for v in values {
                h.append(LINK, v.parse().expect("header"));
            }
            h
        };
        let next = "/api/v1/orgs/x/accounts?limit=2&after=b@b.test";
        assert_eq!(
            next_link(&headers(&[format!("<{next}>; rel=\"next\"").as_str()])),
            Some(next.to_owned())
        );
        assert_eq!(
            next_link(&headers(&[
                "</first>; rel=\"first\"",
                format!("<{next}>; title=\"x\"; rel=\"last next\"").as_str()
            ])),
            Some(next.to_owned())
        );
        assert_eq!(
            next_link(&headers(&["</a>; rel=prev, </b>; rel=next"])),
            Some("/b".to_owned())
        );
        assert_eq!(next_link(&headers(&["</a>; rel=\"prev\""])), None);
        assert_eq!(next_link(&HeaderMap::new()), None);
    }

    /// Отказ Admin API: код, описание и подробность из problem+json, подсказка входа по паролю; тело не JSON —
    /// описание кода HTTP.
    #[test]
    fn problem_details() {
        let body = br#"{"status":401,"title":"Unauthorized","detail":"bad credentials"}"#;
        let Error::Api {
            status,
            problem: p,
            message,
        } = problem(SESSION, StatusCode::UNAUTHORIZED, body)
        else {
            panic!("не Error::Api");
        };
        assert_eq!(status, 401);
        assert_eq!(p.and_then(|p| p.detail).as_deref(), Some("bad credentials"));
        assert!(
            message.starts_with("POST /api/v1/session: 401 Unauthorized: bad credentials")
                && message.contains("неверное имя входа или пароль"),
            "{message}"
        );
        let e = problem(WHOAMI, StatusCode::BAD_GATEWAY, b"<html>");
        assert_eq!(e.to_string(), "GET /api/v1/whoami: 502 Bad Gateway");
    }
}
