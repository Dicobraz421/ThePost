//! Доверенные источники приёма (SRS 4.1, 2.6; модель соединителей приёма Exchange; решение
//! 2026-11-01-transport-scope-external-mta; CONTRACTS.md#Transport).
//!
//! Соединение сопоставляется с [`TrustedSource`] снимка уровня кластера ([`ClusterTransport::source`]) по адресу
//! клиента или имени в сертификате клиента (mTLS: сертификат, проверенный слушателем; без проверки rustls
//! сертификат клиента не отдаёт) — при соединении и повторно после STARTTLS ([`Sources`]).
//!
//! - Шлюз (`SourceKind::Gateway`): команды `XCLIENT` и `XFORWARD` (протокол Postfix) принимаются только от шлюза
//!   с `xclient: true`, от остальных — `550 5.7.0`. `XCLIENT` подменяет клиента сессии (адрес, имя `HELO`) и
//!   возвращает её в начальное состояние (`220`, нужен новый `EHLO`); `XFORWARD` — атрибуты одной транзакции.
//!   SPF, DMARC, экран соединений и лимиты — по исходному клиенту ([`crate::server::Envelope::gateway`]). Без
//!   данных об исходном клиенте при заданном `authserv_id` проверки не повторяются: принимаются результаты шлюза
//!   (`Authentication-Results` с этим authserv-id, иначе ARC с ним — [`crate::AuthCheck::gateway`]).
//! - Приложение (`SourceKind::Application`): отправка без SASL (и на submission), только с разрешённых адресов
//!   отправителя (иначе `550 5.7.1` на `MAIL`), внешние получатели — только при `external` (иначе `550 5.7.1` на
//!   `RCPT`); проверки SPF, DKIM, DMARC не выполняются.
//!
//! Снимок недоступен — соединение обслуживается как от обычного клиента (в журнал).

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tp_contracts::transport::{SourceKind, SourceMatch, TrustedSource};
use tp_platform::Conn;
use tp_platform::listen::Stream;

use crate::routes::Routes;

/// Сопоставление соединений с доверенными источниками по снимку настроек транспорта.
#[derive(Clone)]
pub struct Sources {
    routes: Arc<Routes>,
}

impl fmt::Debug for Sources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sources").finish_non_exhaustive()
    }
}

impl Sources {
    /// Источники из снимков `routes` (перечитываются по сигналу — [`Routes::watch`]).
    pub fn new(routes: Arc<Routes>) -> Self {
        Self { routes }
    }

    /// Доверенный источник соединения `conn`.
    pub fn of(&self, conn: &Conn) -> impl Future<Output = Option<Arc<TrustedSource>>> + Send + '_ {
        // Копия цепочки: ссылка на соединение TLS не держится через `await`.
        let certs: Vec<CertificateDer<'static>> = match &conn.stream {
            Stream::Tls(tls) => tls
                .get_ref()
                .1
                .peer_certificates()
                .unwrap_or_default()
                .iter()
                .map(|c| c.clone().into_owned())
                .collect(),
            Stream::Plain(_) => Vec::new(),
        };
        let ip = conn.info.client.ip();
        async move { self.resolve(ip, &certs).await }
    }

    /// Доверенный источник клиента `ip` (в канонической форме) с проверенной цепочкой сертификатов `certs`
    /// (лист — первый).
    pub async fn resolve(
        &self,
        ip: IpAddr,
        certs: &[CertificateDer<'_>],
    ) -> Option<Arc<TrustedSource>> {
        let cluster = match self.routes.cluster().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "smtp: доверенные источники недоступны, соединение — как от клиента");
                return None;
            }
        };
        let names = cert_names(&cluster.sources, certs);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        cluster.source(ip, &names).cloned().map(Arc::new)
    }
}

/// Имена `ClientCert` источников, на которые выписан сертификат клиента `certs[0]` (dNSName из SAN).
fn cert_names(sources: &[TrustedSource], certs: &[CertificateDer<'_>]) -> Vec<String> {
    let Some(leaf) = certs.first() else {
        return Vec::new();
    };
    let Ok(cert) = webpki::EndEntityCert::try_from(leaf) else {
        return Vec::new();
    };
    sources
        .iter()
        .filter_map(|s| match &s.matcher {
            SourceMatch::ClientCert { subject } => Some(subject),
            SourceMatch::Networks(_) => None,
        })
        .filter(|subject| {
            ServerName::try_from(subject.as_str())
                .is_ok_and(|name| cert.verify_is_valid_for_subject_name(&name).is_ok())
        })
        .cloned()
        .collect()
}

/// Источник — шлюз, которому разрешены `XCLIENT` и `XFORWARD`.
pub(crate) fn xclient_allowed(source: Option<&TrustedSource>) -> bool {
    matches!(
        source.map(|s| &s.kind),
        Some(SourceKind::Gateway { xclient: true, .. })
    )
}

/// authserv-id шлюза, результатам проверок которого доверяем.
pub(crate) fn gateway_authserv(source: Option<&TrustedSource>) -> Option<&str> {
    match source.map(|s| &s.kind) {
        Some(SourceKind::Gateway {
            authserv_id: Some(id),
            ..
        }) => Some(id),
        _ => None,
    }
}

/// Источник — приложение: разрешённые отправители и внешние получатели.
pub(crate) fn application(source: Option<&TrustedSource>) -> Option<(&TrustedSource, bool)> {
    let s = source?;
    match &s.kind {
        SourceKind::Application { external, .. } => Some((s, *external)),
        SourceKind::Gateway { .. } => None,
    }
}

/// Отправитель `mail_from` разрешён источнику-приложению.
pub(crate) fn sender_allowed(source: &TrustedSource, mail_from: &str) -> bool {
    match &source.kind {
        SourceKind::Application { senders, .. } => senders.iter().any(|p| p.matches(mail_from)),
        SourceKind::Gateway { .. } => true,
    }
}

/// Команда расширения шлюза.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum XCommand {
    /// `XCLIENT`: подмена клиента сессии.
    Client,
    /// `XFORWARD`: атрибуты клиента транзакции.
    Forward,
}

/// Атрибуты исходного клиента из `XCLIENT`/`XFORWARD`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Origin {
    /// Адрес (`ADDR`) в канонической форме: `::ffff:a.b.c.d` — адрес IPv4 `a.b.c.d`.
    pub addr: Option<IpAddr>,
    /// Порт (`PORT`).
    pub port: Option<u16>,
    /// Имя `HELO`.
    pub helo: Option<String>,
}

impl Origin {
    /// Атрибуты `other` поверх этих.
    pub(crate) fn merge(&mut self, other: Origin) {
        if other.addr.is_some() {
            self.addr = other.addr;
        }
        if other.port.is_some() {
            self.port = other.port;
        }
        if other.helo.is_some() {
            self.helo = other.helo;
        }
    }

    /// Адрес клиента: исходный адрес с портом (по умолчанию — 0).
    pub(crate) fn socket(&self) -> Option<SocketAddr> {
        self.addr
            .map(|ip| SocketAddr::new(ip, self.port.unwrap_or_default()))
    }
}

/// Строка — команда `XCLIENT` или `XFORWARD` (по началу `head`, без учёта регистра).
pub(crate) fn x_command(head: &[u8]) -> Option<XCommand> {
    let starts = |cmd: &[u8]| {
        head.len() > cmd.len()
            && head[..cmd.len()].eq_ignore_ascii_case(cmd)
            && head[cmd.len()] == b' '
    };
    if starts(b"XCLIENT") {
        Some(XCommand::Client)
    } else if starts(b"XFORWARD") {
        Some(XCommand::Forward)
    } else {
        None
    }
}

/// Наибольшая длина начала строки, по которому распознаётся команда ([`x_command`]).
pub(crate) const X_HEAD: usize = 9;

/// Разбирает аргументы `XCLIENT`/`XFORWARD` (`ИМЯ=значение`, значения — xtext RFC 3461). Неизвестный атрибут или
/// неверный адрес — `Err` с текстом ответа `501`.
pub(crate) fn parse(cmd: XCommand, line: &str) -> Result<Origin, String> {
    let mut out = Origin::default();
    let args = line
        .trim_end_matches(['\r', '\n'])
        .split_once(' ')
        .map_or("", |(_, a)| a);
    for arg in args.split_ascii_whitespace() {
        let (name, value) = arg
            .split_once('=')
            .ok_or_else(|| format!("Bad attribute {arg}"))?;
        let value = xtext(value).ok_or_else(|| format!("Bad value of {name}"))?;
        // Значение недоступно у шлюза: атрибут не задан.
        let unknown = value.eq_ignore_ascii_case("[UNAVAILABLE]")
            || value.eq_ignore_ascii_case("[TEMPUNAVAIL]");
        let name = name.to_ascii_uppercase();
        match (cmd, name.as_str()) {
            (_, "ADDR") if !unknown => {
                let v = value
                    .strip_prefix("IPV6:")
                    .or_else(|| value.strip_prefix("ipv6:"))
                    .unwrap_or(&value);
                // Адрес не проходит через слушатель (PLT-34): каноническая форма — здесь.
                let ip: IpAddr = v.parse().map_err(|_| "Bad ADDR".to_owned())?;
                out.addr = Some(ip.to_canonical());
            }
            (_, "PORT") if !unknown => {
                out.port = Some(value.parse().map_err(|_| "Bad PORT".to_owned())?);
            }
            (_, "HELO") if !unknown => {
                if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
                    return Err("Bad HELO".into());
                }
                out.helo = Some(value);
            }
            (_, "ADDR" | "PORT" | "HELO" | "NAME" | "PROTO")
            | (XCommand::Client, "LOGIN" | "DESTADDR" | "DESTPORT")
            | (XCommand::Forward, "IDENT" | "SOURCE") => {}
            _ => return Err(format!("Bad attribute {name}")),
        }
    }
    Ok(out)
}

/// Декодирует xtext (RFC 3461, 4): `+XX` — байт в шестнадцатеричной записи.
fn xtext(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'+' {
            let hex = [bytes.next()?, bytes.next()?];
            let hex = std::str::from_utf8(&hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

/// Строки `EHLO` для шлюза.
pub(crate) const EHLO_LINES: [&str; 2] = [
    "XCLIENT NAME ADDR PORT PROTO HELO LOGIN DESTADDR DESTPORT",
    "XFORWARD NAME ADDR PORT PROTO HELO IDENT SOURCE",
];

#[cfg(test)]
mod tests;
