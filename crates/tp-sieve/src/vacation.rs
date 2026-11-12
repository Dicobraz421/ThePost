//! Автоответ `vacation` (SIEVE-06; RFC 5230, RFC 3834): решение «отвечать ли» и параметры ответа [`AutoReply`].
//!
//! Источник — действие `vacation` скрипта или настройка учётной записи [`Vacation`] (JMAP `VacationResponse`,
//! Admin API). Ответа нет (RFC 5230 §4.5, RFC 3834 §2): пустой отправитель конверта, `Auto-Submitted` не `no`,
//! заголовки `List-*`, `Precedence: bulk|list|junk`, письмо от собственного адреса, адреса пользователя (получатель
//! конверта без подадреса и `:addresses`) нет среди получателей письма. Период (`:days`, по `:handle`) — через
//! [`VacationLog::seen`]; письмо собирает `tp-smtp`.

use std::time::Duration;

use time::OffsetDateTime;
use tp_contracts::Result;
use tp_contracts::sieve::{
    AutoReply, MAX_VACATION_SUBJECT_LEN, ReplyBody, RulesInput, Vacation, VacationLog,
};

use crate::compile::Part;
use crate::config::VacationConfig;
use crate::run::{Hdr, VacationAction, address_part, parse_addresses, strip_angle};

/// Источник автоответа.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Source<'a> {
    /// Действие `vacation` скрипта.
    Script(&'a VacationAction),
    /// Настройка учётной записи и текущий момент (действует при `from ≤ now < to`).
    Setting(&'a Vacation, OffsetDateTime),
}

/// Заголовки с получателями письма (RFC 5230 §4.5).
const RECIPIENT_HEADERS: &[&str] = &["To", "Cc", "Bcc", "Resent-To", "Resent-Cc", "Resent-Bcc"];

/// `:handle` автоответа из настройки учётной записи.
const SETTING_HANDLE: &str = "vacation-response";

/// Начало темы по умолчанию.
const SUBJECT_PREFIX: &str = "Автоответ";

const DAY: u64 = 24 * 3600;

/// Адрес для сравнения: строчными буквами и без подадреса (`Anna+news@Example.org` → `anna@example.org`).
fn base(addr: &str) -> String {
    let a = strip_angle(addr).to_ascii_lowercase();
    match (address_part(&a, Part::User), address_part(&a, Part::Domain)) {
        (Some(user), Some(domain)) if !domain.is_empty() => format!("{user}@{domain}"),
        _ => a,
    }
}

fn header<'a>(headers: &'a [Hdr], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// Причина не отвечать на письмо; `None` — отвечать можно (период проверяется отдельно).
pub(crate) fn suppressed(
    source: Source<'_>,
    input: &RulesInput,
    headers: &[Hdr],
) -> Option<&'static str> {
    if let Source::Setting(v, now) = source
        && !v.is_active(now)
    {
        return Some("автоответ выключен или вне срока действия");
    }
    if strip_angle(&input.mail_from).is_empty() {
        return Some("пустой отправитель конверта");
    }
    for h in headers {
        let name = h.name.to_ascii_lowercase();
        let value = h.value.trim().to_ascii_lowercase();
        if name == "auto-submitted" {
            let kind = value.split(';').next().unwrap_or_default().trim();
            if kind != "no" {
                return Some("автоматическое письмо (Auto-Submitted)");
            }
        } else if name.starts_with("list-") {
            return Some("письмо списка рассылки (List-*)");
        } else if name == "precedence" && matches!(value.as_str(), "bulk" | "list" | "junk") {
            return Some("массовая рассылка (Precedence)");
        }
    }
    let mut own = vec![base(&input.recipient)];
    if let Source::Script(v) = source {
        own.extend(v.addresses.iter().map(|a| base(a)));
    }
    if own.contains(&base(&input.mail_from)) {
        return Some("письмо от собственного адреса");
    }
    let to_user = headers
        .iter()
        .filter(|h| {
            RECIPIENT_HEADERS
                .iter()
                .any(|n| h.name.eq_ignore_ascii_case(n))
        })
        .flat_map(|h| parse_addresses(input.message.get(h.start..h.end).unwrap_or_default()))
        .any(|a| own.contains(&base(&a)));
    if !to_user {
        return Some("адреса пользователя нет среди получателей письма");
    }
    None
}

/// Решает автоответ: `None` — не отвечать (условия [`suppressed`] или уже отвечали за период). `Err` — сбой журнала.
pub(crate) async fn reply(
    source: Source<'_>,
    input: &RulesInput,
    headers: &[Hdr],
    config: &VacationConfig,
    log: &dyn VacationLog,
) -> Result<Option<AutoReply>> {
    if let Some(reason) = suppressed(source, input, headers) {
        tracing::debug!(account = %input.scope.account, reason, "sieve: автоответ не отправляется");
        return Ok(None);
    }
    let (days, handle) = match source {
        Source::Script(v) => (
            v.days,
            v.handle.clone().unwrap_or_else(|| default_handle(v)),
        ),
        Source::Setting(..) => (None, SETTING_HANDLE.to_owned()),
    };
    let days = days
        .unwrap_or(config.default_days)
        .clamp(config.min_days, config.max_days.max(config.min_days));
    let period = Duration::from_secs(days.saturating_mul(DAY));
    let to = strip_angle(&input.mail_from).to_owned();
    if log
        .seen(input.scope, &to.to_ascii_lowercase(), &handle, period)
        .await?
    {
        return Ok(None);
    }

    let recipient = strip_angle(&input.recipient).to_owned();
    let (from, subject, body) = match source {
        Source::Script(v) => (
            v.from.as_deref().and_then(|f| own_from(f, &recipient)),
            v.subject.clone(),
            if v.mime {
                ReplyBody::Mime(v.reason.clone())
            } else {
                ReplyBody::Text(v.reason.clone())
            },
        ),
        Source::Setting(v, _) => (
            None,
            v.subject.clone(),
            match (&v.text_body, &v.html_body) {
                (text, Some(html)) => ReplyBody::Html {
                    text: text.clone(),
                    html: html.clone(),
                },
                (text, None) => ReplyBody::Text(text.clone().unwrap_or_default()),
            },
        ),
    };
    let subject = subject.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
        match header(headers, "Subject") {
            Some(s) if !s.is_empty() => format!("{SUBJECT_PREFIX}: {s}"),
            _ => SUBJECT_PREFIX.to_owned(),
        }
    });
    let subject: String = subject
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_VACATION_SUBJECT_LEN)
        .collect();

    let message_id = header(headers, "Message-ID").and_then(|v| ids(v).into_iter().next());
    let mut references = header(headers, "References").map(ids).unwrap_or_default();
    if references.is_empty() {
        references = header(headers, "In-Reply-To").map(ids).unwrap_or_default();
    }
    references.extend(message_id.clone());
    Ok(Some(AutoReply {
        to,
        from: from.unwrap_or(recipient),
        subject,
        body,
        in_reply_to: message_id,
        references,
    }))
}

/// Адрес из `:from`, если это адрес получателя (подадрес может отличаться): чужой адрес в `From` скрипт задать не
/// может.
fn own_from(from: &str, recipient: &str) -> Option<String> {
    let addr = parse_addresses(from.as_bytes()).into_iter().next()?;
    (base(&addr) == base(recipient)).then_some(addr)
}

/// Идентификаторы сообщений (`<…>`) из значения заголовка.
fn ids(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .filter(|s| s.len() > 2 && s.starts_with('<') && s.ends_with('>'))
        .map(str::to_owned)
        .collect()
}

/// `:handle` по умолчанию (RFC 5230 §4.7): зависит от `:subject`, `:from`, `:mime` и текста ответа — FNV-1a.
fn default_handle(v: &VacationAction) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let parts = [
        v.subject.as_deref().unwrap_or_default(),
        v.from.as_deref().unwrap_or_default(),
        if v.mime { "mime" } else { "" },
        v.reason.as_str(),
    ];
    for part in parts {
        for b in part.bytes().chain([0]) {
            hash = (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("auto-{hash:016x}")
}
