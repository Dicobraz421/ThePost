//! Уведомления о доставке (DSN, RFC 3464) на `mail-builder`: недоставка, задержка, успешная доставка.
//!
//! Письмо — `multipart/report; report-type=delivery-status` (RFC 6522): пояснение для человека
//! (`text/plain`, по-русски и по-английски), `message/delivery-status` (поля сообщения и по получателю) и
//! исходное письмо — заголовки (`text/rfc822-headers`) или целиком (`message/rfc822`, `RET=FULL`).
//! Отправитель — `MAILER-DAEMON@<hostname>`, конверт — пустой (`<>`): на уведомление уведомления нет.
//! `Auto-Submitted: auto-replied` (RFC 3834).
//!
//! Куда идёт уведомление, решает [`DsnSink`]: [`crate::Delivery`] — локальному отправителю в «Входящие»,
//! внешнему — в очередь; [`crate::queue::Queue`] — всегда в очередь.

use async_trait::async_trait;
use bytes::Bytes;
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_builder::headers::content_type::ContentType;
use mail_builder::headers::date::Date;
use mail_builder::headers::raw::Raw;
use mail_builder::mime::MimePart;
use time::OffsetDateTime;
use tp_contracts::queue::DsnRet;
use tp_contracts::{Result, TenantId};

/// Итог для получателя (`Action`, RFC 3464, 2.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Окончательная недоставка.
    Failed,
    /// Задержка, попытки продолжаются.
    Delayed,
    /// Доставлено (`NOTIFY=SUCCESS`).
    Delivered,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Failed => "failed",
            Action::Delayed => "delayed",
            Action::Delivered => "delivered",
        }
    }
}

/// Получатель в уведомлении.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    /// Адрес (`Final-Recipient`).
    pub address: String,
    /// Исходный получатель (`ORCPT`, с типом: `rfc822;a@b`).
    pub orcpt: Option<String>,
    /// Итог.
    pub action: Action,
    /// Расширенный код (RFC 3463), например `5.1.1`.
    pub status: String,
    /// Ответ сервера (`Diagnostic-Code: smtp; …`) или причина.
    pub reply: String,
}

impl Recipient {
    /// Получатель с кодом из ответа `reply` (`"550 5.1.1 …"`); без кода — `x.0.0` по итогу.
    pub fn new(address: impl Into<String>, action: Action, reply: impl Into<String>) -> Self {
        let reply = reply.into();
        let status = status_of(&reply, action);
        Self {
            address: address.into(),
            orcpt: None,
            action,
            status,
            reply,
        }
    }
}

/// Расширенный код из ответа SMTP.
fn status_of(reply: &str, action: Action) -> String {
    let mut it = reply.split_whitespace();
    let code = it
        .next()
        .filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_digit()));
    if let Some(code) = code {
        if let Some(esc) = it.next().filter(|e| {
            let p: Vec<_> = e.split('.').collect();
            p.len() == 3
                && p.iter()
                    .all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit()))
        }) {
            return esc.to_owned();
        }
        return format!("{}.0.0", &code[..1]);
    }
    match action {
        Action::Failed => "5.0.0".into(),
        Action::Delayed => "4.0.0".into(),
        Action::Delivered => "2.0.0".into(),
    }
}

/// Уведомление о доставке.
#[derive(Debug, Clone, Copy)]
pub struct Report<'a> {
    /// Имя узла (`Reporting-MTA`, адрес `MAILER-DAEMON@…`).
    pub hostname: &'a str,
    /// Кому — отправитель исходного письма.
    pub to: &'a str,
    /// `ENVID` исходного письма.
    pub envid: Option<&'a str>,
    /// Приём исходного письма (`Arrival-Date`).
    pub arrival: OffsetDateTime,
    /// Момент уведомления (`Date`, `Last-Attempt-Date`).
    pub now: OffsetDateTime,
    /// Попытки продолжатся до (`Will-Retry-Until`, задержка).
    pub retry_until: Option<OffsetDateTime>,
    /// Получатели.
    pub rcpts: &'a [Recipient],
    /// Исходное письмо.
    pub original: &'a [u8],
    /// Что вернуть: заголовки или письмо целиком.
    pub ret: DsnRet,
    /// Уникальная часть `Message-ID` и границ MIME (идентификатор записи очереди, транзакции).
    pub id: &'a str,
}

/// Куда отправить уведомление.
#[async_trait]
pub trait DsnSink: Send + Sync + 'static {
    /// Отправляет уведомление `raw` адресату `to` (конверт `<>`).
    async fn send_dsn(&self, tenant: TenantId, to: &str, raw: Bytes) -> Result<()>;
}

fn date(t: OffsetDateTime) -> String {
    Date::new(t.unix_timestamp()).to_rfc822()
}

/// Без управляющих символов (значение поля).
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Блок заголовков исходного письма (с пустой строкой в конце).
fn headers_of(raw: &[u8]) -> &[u8] {
    match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => &raw[..i + 4],
        None => raw,
    }
}

/// Письмо-уведомление (RFC 5322).
pub fn build(r: &Report<'_>) -> Vec<u8> {
    let action = r
        .rcpts
        .iter()
        .map(|x| x.action)
        .find(|a| *a == Action::Failed)
        .or_else(|| r.rcpts.first().map(|x| x.action))
        .unwrap_or(Action::Failed);
    let (subject, ru, en) = match action {
        Action::Failed => (
            "Undelivered Mail Returned to Sender",
            "Письмо не доставлено следующим получателям:",
            "Your message could not be delivered to the following recipients:",
        ),
        Action::Delayed => (
            "Delayed Mail (still being retried)",
            "Письмо пока не доставлено, попытки продолжаются:",
            "Your message has not been delivered yet, delivery is still being retried:",
        ),
        Action::Delivered => (
            "Successful Mail Delivery Report",
            "Письмо доставлено:",
            "Your message was delivered to:",
        ),
    };
    let mut human = format!("{ru}\r\n{en}\r\n\r\n");
    for x in r.rcpts {
        human.push_str(&format!(
            "  <{}>: {}\r\n",
            clean(&x.address),
            clean(&x.reply)
        ));
    }
    if let Some(until) = r.retry_until {
        human.push_str(&format!(
            "\r\nПопытки продолжатся до / Will retry until: {}\r\n",
            date(until)
        ));
    }
    human.push_str(&format!("\r\n-- \r\nThe Post, {}\r\n", r.hostname));

    let mut ds = format!("Reporting-MTA: dns; {}\r\n", r.hostname);
    if let Some(envid) = r.envid {
        ds.push_str(&format!("Original-Envelope-Id: {}\r\n", clean(envid)));
    }
    ds.push_str(&format!("Arrival-Date: {}\r\n", date(r.arrival)));
    for x in r.rcpts {
        ds.push_str("\r\n");
        if let Some(o) = &x.orcpt {
            ds.push_str(&format!("Original-Recipient: {}\r\n", clean(o)));
        }
        ds.push_str(&format!(
            "Final-Recipient: rfc822; {}\r\n",
            clean(&x.address)
        ));
        ds.push_str(&format!("Action: {}\r\n", x.action.as_str()));
        ds.push_str(&format!("Status: {}\r\n", x.status));
        let reply = clean(&x.reply);
        if reply
            .split_whitespace()
            .next()
            .is_some_and(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_digit()))
        {
            ds.push_str(&format!("Diagnostic-Code: smtp; {reply}\r\n"));
        }
        if x.action != Action::Delivered {
            ds.push_str(&format!("Last-Attempt-Date: {}\r\n", date(r.now)));
        }
        if let (Action::Delayed, Some(until)) = (x.action, r.retry_until) {
            ds.push_str(&format!("Will-Retry-Until: {}\r\n", date(until)));
        }
    }

    let (ctype, original): (&str, &[u8]) = match r.ret {
        DsnRet::Full => ("message/rfc822", r.original),
        DsnRet::Headers => ("text/rfc822-headers", headers_of(r.original)),
    };
    let cte = if original.is_ascii() { "7bit" } else { "8bit" };
    let report = MimePart::new(
        ContentType::new("multipart/report")
            .attribute("report-type", "delivery-status")
            .attribute("boundary", format!("dsn-{}", r.id)),
        vec![
            MimePart::new("text/plain", human),
            MimePart::new("message/delivery-status", ds.into_bytes()).transfer_encoding("7bit"),
            MimePart::new(ctype, original).transfer_encoding(cte),
        ],
    );
    let from = format!("MAILER-DAEMON@{}", r.hostname);
    MessageBuilder::new()
        .from(Address::new_address(Some("Mail Delivery System"), from))
        .to(Address::new_address(None::<&str>, r.to.to_owned()))
        .subject(subject)
        .date(Date::new(r.now.unix_timestamp()))
        .message_id(format!("dsn-{}@{}", r.id, r.hostname))
        .header("Auto-Submitted", Raw::new("auto-replied"))
        .body(report)
        .write_to_vec()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
