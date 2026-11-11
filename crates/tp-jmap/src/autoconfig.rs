//! Автонастройка клиентов (SRS 4.2, решение 2026-11-04): пользователь вводит адрес и пароль, клиент находит серверы сам.
//!
//! - `/.well-known/caldav`, `/.well-known/carddav` — перенаправление 301 на контекст CalDAV и CardDAV (RFC 6764 §5),
//!   любой метод (клиенты Apple начинают с `PROPFIND`), без аутентификации;
//! - `/mail/config-v1.1.xml` (имя `autoconfig.<домен>`) и `/.well-known/autoconfig/mail/config-v1.1.xml` — автонастройка
//!   Thunderbird: IMAP, SMTP submission, CardDAV и CalDAV; домен — из `?emailaddress=` или из имени `autoconfig.<домен>`,
//!   без аутентификации (ответ не зависит от наличия учётной записи);
//! - `/mail/profile.mobileconfig` — профиль iOS и macOS (почта, календари, контакты) для вошедшего пользователя
//!   (Basic или Bearer, как API); пароль в профиль не входит — устройство спросит его при установке. Профиль без подписи.
//!
//! `/.well-known/jmap` — объект сессии (`crate::core::http`). Серверы и порты — секция `jmap.autoconfig`
//! ([`AutoconfigConfig`](crate::core::config::AutoconfigConfig)).

use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::core::Jmap;
use crate::core::config::{MailEndpoint, Security};
use crate::core::http::auth_error;

/// Тип содержимого профиля Apple.
const MOBILECONFIG: &str = "application/x-apple-aspen-config";

/// Серверы для ответа автонастройки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Servers {
    /// Имя сервера IMAP и SMTP.
    pub host: String,
    /// Адрес контекста CalDAV.
    pub caldav: String,
    /// Адрес контекста CardDAV.
    pub carddav: String,
    /// IMAP.
    pub imap: MailEndpoint,
    /// SMTP submission.
    pub submission: MailEndpoint,
    /// Название организации; `None` — домен.
    pub organization: Option<String>,
}

impl Jmap {
    /// Серверы по конфигурации и заголовку `Host`: имя — `autoconfig.host`, иначе узел `public_url`,
    /// иначе `Host` без порта и префикса `autoconfig.`; адреса DAV — от `public_url` или `https://<имя>`.
    pub(crate) fn servers(&self, headers: &HeaderMap) -> Servers {
        let a = &self.cfg.autoconfig;
        let public = self
            .cfg
            .public_url
            .as_deref()
            .map(|u| u.trim_end_matches('/'));
        let host = a
            .host
            .clone()
            .or_else(|| public.and_then(|u| url_parts(u).map(|p| p.host)))
            .unwrap_or_else(|| {
                let h = request_host(headers);
                strip_autoconfig(&h).to_owned()
            });
        let base = public.map_or_else(|| format!("https://{host}"), str::to_owned);
        Servers {
            caldav: join(&base, &a.caldav),
            carddav: join(&base, &a.carddav),
            host,
            imap: a.imap.clone(),
            submission: a.submission.clone(),
            organization: a.organization.clone(),
        }
    }
}

/// Параметры запроса автонастройки Thunderbird.
#[derive(Debug, Deserialize)]
pub(crate) struct AutoconfigQuery {
    emailaddress: Option<String>,
}

/// `/.well-known/caldav`: 301 на контекст CalDAV.
pub(crate) async fn caldav(State(j): State<Arc<Jmap>>, headers: HeaderMap) -> Response {
    redirect(&j.servers(&headers).caldav)
}

/// `/.well-known/carddav`: 301 на контекст CardDAV.
pub(crate) async fn carddav(State(j): State<Arc<Jmap>>, headers: HeaderMap) -> Response {
    redirect(&j.servers(&headers).carddav)
}

/// Автонастройка Thunderbird; домен не определён или недопустим — 400.
pub(crate) async fn thunderbird(
    State(j): State<Arc<Jmap>>,
    Query(q): Query<AutoconfigQuery>,
    headers: HeaderMap,
) -> Response {
    let domain = match q.emailaddress.as_deref() {
        Some(addr) => addr.rsplit_once('@').map(|(_, d)| d.to_owned()),
        None => Some(strip_autoconfig(&request_host(&headers)).to_owned()),
    };
    let Some(domain) = domain.and_then(|d| valid_domain(&d)) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let body = thunderbird_xml(&domain, &j.servers(&headers));
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

/// Профиль `.mobileconfig` вошедшего пользователя.
pub(crate) async fn mobileconfig(State(j): State<Arc<Jmap>>, headers: HeaderMap) -> Response {
    let p = match j.authenticate(&headers).await {
        Ok(p) => p,
        Err(e) => return auth_error(&e),
    };
    let name = match j.d.accounts.get(p.tenant, p.account).await {
        Ok(Some(a)) if !a.display_name.is_empty() => a.display_name,
        _ => p.login.clone(),
    };
    let servers = j.servers(&headers);
    let domain = p
        .login
        .rsplit_once('@')
        .and_then(|(_, d)| valid_domain(d))
        .unwrap_or_else(|| servers.host.clone());
    let body = mobileconfig_plist(&p.login, &name, &domain, &servers);
    let disposition =
        HeaderValue::from_str(&format!("attachment; filename=\"{domain}.mobileconfig\""))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(MOBILECONFIG)),
            (header::CONTENT_DISPOSITION, disposition),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache, no-store"),
            ),
        ],
        body,
    )
        .into_response()
}

fn redirect(location: &str) -> Response {
    match HeaderValue::from_str(location) {
        Ok(v) => (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, v)]).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Документ автонастройки Thunderbird (`clientConfig` 1.1): IMAP, SMTP, CardDAV, CalDAV.
pub(crate) fn thunderbird_xml(domain: &str, s: &Servers) -> String {
    let org = esc(s.organization.as_deref().unwrap_or(domain));
    let domain = esc(domain);
    let host = esc(&s.host);
    let server = |kind: &str, tag: &str, e: &MailEndpoint| {
        let socket = match e.security {
            Security::Tls => "SSL",
            Security::Starttls => "STARTTLS",
        };
        format!(
            "    <{tag} type=\"{kind}\">\n\
             \x20     <hostname>{host}</hostname>\n\
             \x20     <port>{port}</port>\n\
             \x20     <socketType>{socket}</socketType>\n\
             \x20     <authentication>password-cleartext</authentication>\n\
             \x20     <username>%EMAILADDRESS%</username>\n\
             \x20   </{tag}>\n",
            port = e.port,
        )
    };
    let dav = |tag: &str, kind: &str, url: &str| {
        format!(
            "  <{tag} type=\"{kind}\">\n\
             \x20   <username>%EMAILADDRESS%</username>\n\
             \x20   <authentication>http-basic</authentication>\n\
             \x20   <serverURL>{url}</serverURL>\n\
             \x20 </{tag}>\n",
            url = esc(url),
        )
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <clientConfig version=\"1.1\">\n\
         \x20 <emailProvider id=\"{domain}\">\n\
         \x20   <domain>{domain}</domain>\n\
         \x20   <displayName>{org}</displayName>\n\
         \x20   <displayShortName>{org}</displayShortName>\n\
         {imap}{smtp}\
         \x20 </emailProvider>\n\
         {carddav}{caldav}\
         </clientConfig>\n",
        imap = server("imap", "incomingServer", &s.imap),
        smtp = server("smtp", "outgoingServer", &s.submission),
        carddav = dav("addressBook", "carddav", &s.carddav),
        caldav = dav("calendar", "caldav", &s.caldav),
    )
}

/// Значение списка свойств Apple (plist).
enum Plist {
    Str(String),
    Int(u64),
    Bool(bool),
    Dict(Vec<(&'static str, Plist)>),
    Array(Vec<Plist>),
}

impl Plist {
    fn str(s: impl Into<String>) -> Self {
        Self::Str(s.into())
    }

    fn write(&self, out: &mut String, depth: usize) {
        let pad = "\t".repeat(depth);
        match self {
            Self::Str(s) => {
                let _ = writeln!(out, "{pad}<string>{}</string>", esc(s));
            }
            Self::Int(n) => {
                let _ = writeln!(out, "{pad}<integer>{n}</integer>");
            }
            Self::Bool(b) => {
                let _ = writeln!(out, "{pad}<{b}/>");
            }
            Self::Dict(items) => {
                let _ = writeln!(out, "{pad}<dict>");
                for (k, v) in items {
                    let _ = writeln!(out, "{pad}\t<key>{k}</key>");
                    v.write(out, depth + 1);
                }
                let _ = writeln!(out, "{pad}</dict>");
            }
            Self::Array(items) => {
                let _ = writeln!(out, "{pad}<array>");
                for v in items {
                    v.write(out, depth + 1);
                }
                let _ = writeln!(out, "{pad}</array>");
            }
        }
    }
}

/// Профиль `.mobileconfig`: учётные записи почты (IMAP, SMTP), CalDAV и CardDAV. Идентификаторы и UUID
/// выводятся из имени сервера и входа: повторная установка заменяет профиль, а не добавляет второй.
pub(crate) fn mobileconfig_plist(login: &str, name: &str, domain: &str, s: &Servers) -> String {
    let org = s.organization.clone().unwrap_or_else(|| domain.to_owned());
    let prefix = format!(
        "{}.thepost.{}",
        s.host.split('.').rev().collect::<Vec<_>>().join("."),
        &crate::core::digest(login.to_lowercase().as_bytes())[..16]
    );
    let head = |kind: &str, ty: &str, display: String| {
        let id = format!("{prefix}.{kind}");
        vec![
            ("PayloadType", Plist::str(ty)),
            ("PayloadVersion", Plist::Int(1)),
            ("PayloadIdentifier", Plist::str(id.clone())),
            ("PayloadUUID", Plist::Str(uuid(&id))),
            ("PayloadDisplayName", Plist::Str(display)),
        ]
    };
    let mut mail = head("mail", "com.apple.mail.managed", format!("Почта {org}"));
    mail.extend([
        ("EmailAccountDescription", Plist::str(org.clone())),
        ("EmailAccountName", Plist::str(name)),
        ("EmailAccountType", Plist::str("EmailTypeIMAP")),
        ("EmailAddress", Plist::str(login)),
        (
            "IncomingMailServerAuthentication",
            Plist::str("EmailAuthPassword"),
        ),
        ("IncomingMailServerHostName", Plist::str(s.host.clone())),
        (
            "IncomingMailServerPortNumber",
            Plist::Int(s.imap.port.into()),
        ),
        ("IncomingMailServerUseSSL", Plist::Bool(true)),
        ("IncomingMailServerUsername", Plist::str(login)),
        (
            "OutgoingMailServerAuthentication",
            Plist::str("EmailAuthPassword"),
        ),
        ("OutgoingMailServerHostName", Plist::str(s.host.clone())),
        (
            "OutgoingMailServerPortNumber",
            Plist::Int(s.submission.port.into()),
        ),
        ("OutgoingMailServerUseSSL", Plist::Bool(true)),
        ("OutgoingMailServerUsername", Plist::str(login)),
        ("OutgoingPasswordSameAsIncomingPassword", Plist::Bool(true)),
    ]);
    // Ключи DAV: описание, узел, порт, TLS, вход, адрес принципала (префикс `CalDAV` или `CardDAV`).
    let dav = |kind: &str, ty: &str, keys: [&'static str; 6], url: &str, what: &str| {
        let parts = url_parts(url).unwrap_or_else(|| UrlParts {
            host: s.host.clone(),
            port: 443,
            tls: true,
        });
        let mut d = head(kind, ty, format!("{what} {org}"));
        let [desc, host, port, tls, user, principal] = keys;
        d.extend([
            (desc, Plist::Str(format!("{what} {org}"))),
            (host, Plist::Str(parts.host)),
            (port, Plist::Int(parts.port.into())),
            (tls, Plist::Bool(parts.tls)),
            (user, Plist::str(login)),
            (principal, Plist::str(url)),
        ]);
        Plist::Dict(d)
    };
    let caldav = dav(
        "caldav",
        "com.apple.caldav.account",
        [
            "CalDAVAccountDescription",
            "CalDAVHostName",
            "CalDAVPort",
            "CalDAVUseSSL",
            "CalDAVUsername",
            "CalDAVPrincipalURL",
        ],
        &s.caldav,
        "Календари",
    );
    let carddav = dav(
        "carddav",
        "com.apple.carddav.account",
        [
            "CardDAVAccountDescription",
            "CardDAVHostName",
            "CardDAVPort",
            "CardDAVUseSSL",
            "CardDAVUsername",
            "CardDAVPrincipalURL",
        ],
        &s.carddav,
        "Контакты",
    );
    let mut top = head(
        "profile",
        "Configuration",
        format!("Почта, календари и контакты {org}"),
    );
    top.extend([
        (
            "PayloadDescription",
            Plist::Str(format!(
                "Настройка почты, календарей и контактов {login}. Пароль запрашивается при установке."
            )),
        ),
        ("PayloadOrganization", Plist::str(org.clone())),
        ("PayloadRemovalDisallowed", Plist::Bool(false)),
        (
            "PayloadContent",
            Plist::Array(vec![Plist::Dict(mail), caldav, carddav]),
        ),
    ]);
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n",
    );
    Plist::Dict(top).write(&mut out, 0);
    out.push_str("</plist>\n");
    out
}

/// UUID из имени: первые 16 байт SHA-256, версия 8 и вариант RFC 9562.
fn uuid(name: &str) -> String {
    let h = Sha256::digest(name.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&h[..16]);
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Экранирование XML.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Домен: буквы, цифры, `-`, `.`, не длиннее 253 символов; в нижнем регистре.
fn valid_domain(d: &str) -> Option<String> {
    let d = d.trim_end_matches('.');
    let ok = !d.is_empty()
        && d.len() <= 253
        && d.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    ok.then(|| d.to_ascii_lowercase())
}

/// Имя узла из заголовка `Host` без порта; нет заголовка — `localhost`.
fn request_host(headers: &HeaderMap) -> String {
    let h = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    split_port(h).0.to_ascii_lowercase()
}

/// Имя без префикса `autoconfig.`.
fn strip_autoconfig(host: &str) -> &str {
    host.strip_prefix("autoconfig.").unwrap_or(host)
}

/// Узел и порт: `[::1]:8443` → (`[::1]`, `Some(8443)`).
fn split_port(authority: &str) -> (&str, Option<u16>) {
    let colon = if authority.starts_with('[') {
        authority
            .find(']')
            .and_then(|i| authority[i..].find(':').map(|j| i + j))
    } else {
        authority.rfind(':')
    };
    match colon {
        Some(i) => match authority[i + 1..].parse() {
            Ok(port) => (&authority[..i], Some(port)),
            Err(_) => (authority, None),
        },
        None => (authority, None),
    }
}

/// Части адреса `http(s)://узел[:порт]/…`.
#[derive(Debug, PartialEq, Eq)]
struct UrlParts {
    host: String,
    port: u16,
    tls: bool,
}

fn url_parts(url: &str) -> Option<UrlParts> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else {
        (false, url.strip_prefix("http://")?)
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let (host, port) = split_port(authority);
    if host.is_empty() {
        return None;
    }
    Some(UrlParts {
        host: host.to_ascii_lowercase(),
        port: port.unwrap_or(if tls { 443 } else { 80 }),
        tls,
    })
}

/// Полный адрес: значение с `http://` или `https://` — как есть, иначе путь от `base`.
fn join(base: &str, path: &str) -> String {
    if path.starts_with("https://") || path.starts_with("http://") {
        path.to_owned()
    } else if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

#[cfg(test)]
mod tests;
