//! Содержимое писем в списках Admin API (SRS 5.2, 4.7; ADM-19): тема и отправитель писем корзины и очереди модерации.
//!
//! Содержимое отдаётся только по явному параметру запроса `include=subject` и только администратору организации
//! или инсталляции (оператору «только чтение» — 403). Каждый такой запрос — запись журнала аудита о доступе к
//! содержимому: кто, чей список (путь запроса), сколько писем ([`AuditRecord::content_messages`]); самих тем в
//! записи и в `tracing` нет. Запись не сохранена — содержимое не отдаётся.
//!
//! Значения — из заголовков письма (`Subject`, `From`), декодированные (`tp_mailstore::meta`), без управляющих
//! символов, обрезанные до `admin.subject_max_chars`; читается только начало письма (`admin.header_scan_bytes`).
//! Нет заголовка или письмо недоступно — `null`.

use axum::body::Body;
use axum::http::Method;
use time::OffsetDateTime;
use tp_contracts::blob::ByteStream;
use tp_contracts::{Error, Result, TenantId};

use crate::audit::AuditRecord;
use crate::server::{Admin, AdminConfig, Caller};

/// Значение параметра `include`, включающее тему и отправителя.
const SUBJECT: &str = "subject";

/// Запрос просит содержимое: `include=subject`; другое значение — `Invalid`.
pub(crate) fn requested(include: Option<&str>) -> Result<bool> {
    match include {
        None => Ok(false),
        Some(SUBJECT) => Ok(true),
        Some(other) => Err(Error::invalid(format!(
            "include: неизвестное значение {other:?} (допустимо: {SUBJECT})"
        ))),
    }
}

/// Запрос списка с содержимым: кто и какой список просит.
pub(crate) struct Access<'a> {
    /// Администратор.
    pub caller: &'a Caller,
    /// Организация списка.
    pub org: TenantId,
    /// Шаблон маршрута.
    pub route: &'a str,
    /// Путь запроса (в нём — чья корзина).
    pub path: &'a str,
}

impl Access<'_> {
    /// Содержимое — только администратору организации: тому, кому в ней разрешены изменения. Отказ пишется в аудит.
    pub(crate) async fn check(&self, a: &Admin) -> Result<()> {
        if self
            .caller
            .permits(Some(self.org), &Method::POST, self.route)
        {
            return Ok(());
        }
        self.record(a, 403, 0).await?;
        Err(Error::Forbidden)
    }

    /// Запись аудита о доступе к содержимому `messages` писем; не сохранена — ошибка (содержимое не отдаётся).
    pub(crate) async fn record(&self, a: &Admin, status: u16, messages: usize) -> Result<()> {
        let p = &self.caller.principal;
        let record = AuditRecord {
            at: OffsetDateTime::now_utc(),
            tenant: Some(self.org),
            actor_tenant: p.tenant,
            actor: p.account,
            actor_login: p.login.clone(),
            roles: self.caller.roles.iter().copied().collect(),
            method: Method::GET.to_string(),
            route: self.route.to_owned(),
            path: self.path.to_owned(),
            status,
            content_messages: Some(u64::try_from(messages).unwrap_or(u64::MAX)),
        };
        a.d.audit.record(&record).await
    }
}

/// Тема и отправитель письма.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Headline {
    /// Заголовок `Subject`.
    pub subject: Option<String>,
    /// Заголовок `From`: `Имя <адрес>` или адрес; несколько — через запятую.
    pub from: Option<String>,
}

/// Диапазон начала письма, в котором ищутся заголовки.
pub(crate) fn head_range(cfg: &AdminConfig) -> std::ops::Range<u64> {
    0..u64::try_from(cfg.header_scan_bytes).unwrap_or(u64::MAX)
}

/// Тема и отправитель по началу письма ([`head_range`]); письмо недоступно — пусто.
pub(crate) async fn headline(cfg: &AdminConfig, head: Result<ByteStream>) -> Headline {
    let Ok(stream) = head else {
        return Headline::default();
    };
    match axum::body::to_bytes(Body::from_stream(stream), cfg.header_scan_bytes).await {
        Ok(raw) => parse(&raw, cfg.subject_max_chars),
        Err(_) => Headline::default(),
    }
}

/// Разбор заголовков начала письма; значения — до `max` символов.
fn parse(raw: &[u8], max: usize) -> Headline {
    let end = [&b"\r\n\r\n"[..], b"\n\n"]
        .iter()
        .filter_map(|sep| {
            // Вместе с разделителем: без перевода строки последний заголовок не разбирается.
            let at = raw.windows(sep.len()).position(|w| w == *sep)?;
            Some(at + sep.len())
        })
        .min()
        .unwrap_or(raw.len());
    let env = tp_mailstore::meta::message_meta(&raw[..end]).envelope;
    let from: Vec<String> = env
        .from
        .iter()
        .map(|a| match a.name.as_deref().map(str::trim) {
            Some(n) if !n.is_empty() => format!("{n} <{}>", a.email),
            _ => a.email.clone(),
        })
        .collect();
    Headline {
        subject: env.subject.as_deref().and_then(|s| clip(s, max)),
        from: clip(&from.join(", "), max),
    }
}

/// Одна строка без управляющих символов, до `max` символов; пусто — `None`.
fn clip(s: &str, max: usize) -> Option<String> {
    let line: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let out: String = line.trim().chars().take(max).collect();
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::{Headline, clip, parse, requested};

    #[test]
    fn include_values() {
        assert!(!requested(None).expect("none"));
        assert!(requested(Some("subject")).expect("subject"));
        assert!(requested(Some("body")).is_err());
        assert!(requested(Some("")).is_err());
    }

    #[test]
    fn headers_decoded_and_clipped() {
        let raw = "From: =?UTF-8?B?0JDQvdC90LA=?= <anna@example.org>\r\n\
                   Subject: =?koi8-r?B?8NLJ18XU?=\r\n\tmir\r\n\r\nSubject: body\r\n";
        let h = parse(raw.as_bytes(), 100);
        assert_eq!(h.subject.as_deref(), Some("Привет mir"));
        assert_eq!(h.from.as_deref(), Some("Анна <anna@example.org>"));
        let h = parse(b"From: a@example.org, B <b@example.org>\n\nx", 100);
        assert_eq!(h.from.as_deref(), Some("a@example.org, B <b@example.org>"));
        assert_eq!(h.subject, None);
        assert_eq!(parse(b"", 100), Headline::default());
        assert_eq!(parse(b"\xff\xfe no headers", 100), Headline::default());
        let h = parse("Subject: абвгд\n\n".as_bytes(), 3);
        assert_eq!(h.subject.as_deref(), Some("абв"));
        assert_eq!(clip(" a\u{7}b \n", 10).as_deref(), Some("a b"));
        assert_eq!(clip(" \n ", 10), None);
    }
}
