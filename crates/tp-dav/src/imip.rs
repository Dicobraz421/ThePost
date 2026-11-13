//! iMIP (RFC 6047): iTIP внешним участникам письмом — реализация [`ItipSender`] поверх [`Outbound`].
//!
//! Письмо ([`message`]): `multipart/mixed` из `multipart/alternative` (`text/plain` для клиентов без календаря и
//! `text/calendar; method=…` — по нему почтовые клиенты показывают кнопки ответа) и вложения `invite.ics`
//! (`application/ics`, для клиентов, которые читают только вложения). `From` и отправитель конверта — адрес
//! организатора или участника; подпись DKIM и доставка — в очереди ([`Outbound::submit`]). Тему и текст по
//! сообщению iTIP строит [`describe`].

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Bytes;
use calcard::icalendar::{ICalendar, ICalendarParameterName, ICalendarProperty};
use mail_builder::MessageBuilder;
use mail_builder::headers::content_type::ContentType;
use mail_builder::mime::MimePart;
use time::OffsetDateTime;
use tp_contracts::queue::{Outbound, Submitted};
use tp_contracts::schedule::{ItipMethod, ItipOutgoing, ItipSender};
use tp_contracts::{Error, Result};

use crate::itip;

/// Отправка iMIP через очередь исходящей почты.
#[derive(Clone)]
pub struct ImipSender {
    outbound: Arc<dyn Outbound>,
}

impl std::fmt::Debug for ImipSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImipSender").finish_non_exhaustive()
    }
}

impl ImipSender {
    /// Отправка через `outbound` (очередь с подписью DKIM).
    pub fn new(outbound: Arc<dyn Outbound>) -> Self {
        Self { outbound }
    }
}

/// Письмо iMIP (RFC 5322) с датой `now`.
pub fn message(msg: &ItipOutgoing, now: OffsetDateTime) -> Result<Vec<u8>> {
    let domain = msg.from.rsplit_once('@').map_or("localhost", |(_, d)| d);
    let stamp = now.unix_timestamp_nanos().to_string();
    let id = itip::hash_hex(
        &[msg.ics.clone(), msg.to.join(","), stamp]
            .into_iter()
            .collect(),
    );
    let calendar = || {
        ContentType::new("text/calendar")
            .attribute("method", msg.method.as_str())
            .attribute("charset", "utf-8")
    };
    let body = MimePart::new(
        "multipart/mixed",
        vec![
            MimePart::new(
                "multipart/alternative",
                vec![
                    MimePart::new("text/plain", msg.text.as_str()),
                    MimePart::new(calendar(), msg.ics.as_str()),
                ],
            ),
            MimePart::new(
                ContentType::new("application/ics").attribute("name", "invite.ics"),
                msg.ics.as_bytes(),
            )
            .attachment("invite.ics"),
        ],
    );
    MessageBuilder::new()
        .from(msg.from.as_str())
        .to(msg.to.iter().map(String::as_str).collect::<Vec<_>>())
        .subject(msg.subject.as_str())
        .date(now.unix_timestamp())
        .message_id(format!("{id}@{domain}"))
        .body(body)
        .write_to_vec()
        .map_err(|e| Error::internal(format!("сборка письма iMIP: {e}")))
}

/// Начало первого компонента: «05.01.2026 10:00» и пояс (`TZID` или UTC).
fn when(ical: &ICalendar) -> Option<String> {
    let e = ical
        .components
        .iter()
        .filter(|c| itip::is_sched(c))
        .find_map(|c| c.property(&ICalendarProperty::Dtstart))?;
    let dt = e.values.first()?.as_partial_date_time()?;
    let mut out = format!("{:02}.{:02}.{:04}", dt.day?, dt.month?, dt.year?);
    if let (Some(h), Some(m)) = (dt.hour, dt.minute) {
        out.push_str(&format!(" {h:02}:{m:02}"));
        match e
            .parameter(&ICalendarParameterName::Tzid)
            .and_then(|v| v.as_text())
        {
            Some(tz) => out.push_str(&format!(" ({tz})")),
            None if dt.tz_hour.is_some() => out.push_str(" UTC"),
            None => {}
        }
    }
    Some(out)
}

/// Тема и текст письма iMIP для сообщения `ical` с методом `method`.
pub fn describe(method: ItipMethod, ical: &ICalendar) -> (String, String) {
    let m = itip::meeting(ical);
    let partstat = m
        .as_ref()
        .and_then(|m| m.attendees.first())
        .map(|a| a.partstat.as_str());
    let prefix = match (method, partstat) {
        (ItipMethod::Request, _) => "Приглашение",
        (ItipMethod::Cancel, _) => "Отменено",
        (ItipMethod::Reply, Some("ACCEPTED")) => "Принято",
        (ItipMethod::Reply, Some("DECLINED")) => "Отклонено",
        (ItipMethod::Reply, Some("TENTATIVE")) => "Под вопросом",
        (ItipMethod::Reply, _) => "Ответ",
        (ItipMethod::Counter, _) => "Предложено другое время",
        _ => "Календарь",
    };
    let summary: String = m
        .as_ref()
        .and_then(|m| m.summary.clone())
        .unwrap_or_else(|| "(без темы)".to_owned())
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let subject = format!("{prefix}: {summary}");
    let mut text = format!("{subject}\r\n");
    if let Some(w) = when(ical) {
        text.push_str(&format!("Когда: {w}\r\n"));
    }
    if let Some(o) = m.as_ref().and_then(|m| m.organizer.as_deref()) {
        text.push_str(&format!("Организатор: {o}\r\n"));
    }
    if method == ItipMethod::Reply
        && let Some(a) = m.as_ref().and_then(|m| m.attendees.first())
    {
        text.push_str(&format!("Участник: {}\r\n", a.address));
    }
    (subject, text)
}

#[async_trait]
impl ItipSender for ImipSender {
    async fn send(&self, msg: &ItipOutgoing) -> Result<Submitted> {
        msg.validate()?;
        let raw = message(msg, OffsetDateTime::now_utc())?;
        self.outbound
            .submit(
                msg.sender.tenant,
                Some(msg.sender.account),
                &msg.from,
                &msg.to,
                Bytes::from(raw),
            )
            .await
    }
}
