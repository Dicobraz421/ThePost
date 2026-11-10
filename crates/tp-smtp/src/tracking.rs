//! Журнал доставки по модели message tracking Exchange (SRS 5.2, CONTRACTS.md#DeliveryLog).
//!
//! [`Tracking`] записывает в [`DeliveryLog`] события письма: приём (`Receive`), отказ с причиной (`Reject`:
//! на `RCPT`, на `DATA`, получателю после приёма), доставку в ящики и постановку в очередь модерации
//! (`Deliver`, решение `Hold`), передачу наружу (`Relay`, роль smtp-out) и уведомления о доставке (`Dsn`).
//! Записываются отправитель и получатели конверта, Message-ID, решение и причина (ответ SMTP); содержимое
//! писем не записывается. Сбой журнала не останавливает почту: событие теряется с предупреждением в журнале узла.
//!
//! Подключение: [`crate::Delivery::with_tracking`] (smtp-in, submission, отправка вне сессии) и
//! [`crate::QueueWorker::with_tracking`] (smtp-out).

use std::fmt;
use std::sync::Arc;

use time::OffsetDateTime;
use tp_contracts::delivery::{DeliveryDecision, DeliveryEvent, DeliveryEventKind, DeliveryLog};
use tp_contracts::membership::NodeId;
use tp_contracts::{QueueId, TenantId};

use crate::server::Reply;

/// Запись событий в журнал доставки от имени узла.
#[derive(Clone)]
pub struct Tracking {
    log: Arc<dyn DeliveryLog>,
    node: NodeId,
}

impl fmt::Debug for Tracking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tracking")
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

/// Событие письма до записи: общие поля (организация, время, Message-ID) добавляет [`Tracking::write`].
#[derive(Debug, Clone)]
pub(crate) struct Step {
    pub kind: DeliveryEventKind,
    pub decision: DeliveryDecision,
    /// Отправитель события; `None` — отправитель конверта.
    pub sender: Option<String>,
    pub recipients: Vec<String>,
    pub reason: Option<String>,
}

impl Step {
    pub(crate) fn new(
        kind: DeliveryEventKind,
        decision: DeliveryDecision,
        recipients: Vec<String>,
    ) -> Self {
        Self {
            kind,
            decision,
            sender: None,
            recipients,
            reason: None,
        }
    }

    pub(crate) fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub(crate) fn sender(mut self, sender: impl Into<String>) -> Self {
        self.sender = Some(sender.into());
        self
    }
}

/// События одного письма, накопленные при обработке.
#[derive(Debug, Default)]
pub(crate) struct Trail {
    /// Message-ID (после `DATA`).
    pub message_id: Option<String>,
    /// Решение о приёме, если не `Accept` (карантин, отброс фильтром).
    pub decision: Option<DeliveryDecision>,
    pub steps: Vec<Step>,
}

/// Общие поля событий одного письма.
pub(crate) struct Mail<'a> {
    pub tenant: TenantId,
    pub sender: &'a str,
    pub message_id: Option<&'a str>,
    pub queue_id: Option<QueueId>,
    pub at: OffsetDateTime,
}

impl Tracking {
    /// Журнал `log`; события помечаются узлом `node`.
    pub fn new(log: Arc<dyn DeliveryLog>, node: NodeId) -> Self {
        Self { log, node }
    }

    /// Записывает события `steps` письма `mail` одной пачкой; сбой — только предупреждение.
    pub(crate) async fn write(&self, mail: &Mail<'_>, steps: Vec<Step>) {
        if steps.is_empty() {
            return;
        }
        let events = steps
            .into_iter()
            .map(|s| DeliveryEvent {
                tenant: mail.tenant,
                at: mail.at,
                node: self.node.clone(),
                kind: s.kind,
                sender: s.sender.unwrap_or_else(|| mail.sender.to_owned()),
                recipients: s.recipients,
                message_id: mail.message_id.map(str::to_owned),
                queue_id: mail.queue_id,
                decision: s.decision,
                reason: s.reason,
            })
            .collect();
        if let Err(e) = self.log.record_batch(events).await {
            tracing::warn!(error = %e, "smtp: событие журнала доставки не записано");
        }
    }
}

/// Решение по ответу SMTP: `2xx` — принято, `4xx` — отложено, иначе — отказ.
pub(crate) fn decision(r: &Reply) -> DeliveryDecision {
    match r.code {
        200..=299 => DeliveryDecision::Accept,
        400..=499 => DeliveryDecision::Defer,
        _ => DeliveryDecision::Reject,
    }
}

/// Ответ SMTP строкой для причины: `550 5.1.1 Mailbox unavailable`.
pub(crate) fn reason(r: &Reply) -> String {
    let [a, b, c] = r.esc;
    if a == 0 {
        format!("{} {}", r.code, r.message)
    } else {
        format!("{} {a}.{b}.{c} {}", r.code, r.message)
    }
}

/// Решение по строке ответа (`"550 5.1.1 …"`): по первой цифре кода.
pub(crate) fn decision_of_line(reply: &str) -> DeliveryDecision {
    match reply.as_bytes().first() {
        Some(b'2') => DeliveryDecision::Accept,
        Some(b'4') => DeliveryDecision::Defer,
        _ => DeliveryDecision::Reject,
    }
}

/// Message-ID письма без угловых скобок (заголовки письма; нет или не разобран — `None`).
pub(crate) fn message_id(raw: &[u8]) -> Option<String> {
    mail_parser::MessageParser::new()
        .parse_headers(raw)
        .and_then(|m| m.message_id().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::reply;

    #[test]
    fn reply_reason_and_decision() {
        let r = reply(550, [5, 1, 1], "Mailbox unavailable");
        assert_eq!(reason(&r), "550 5.1.1 Mailbox unavailable");
        assert_eq!(decision(&r), DeliveryDecision::Reject);
        assert_eq!(
            decision(&reply(452, [4, 2, 2], "x")),
            DeliveryDecision::Defer
        );
        assert_eq!(reason(&reply(250, [0, 0, 0], "OK")), "250 OK");
        assert_eq!(decision_of_line("451 4.3.0 x"), DeliveryDecision::Defer);
        assert_eq!(
            decision_of_line("delivery time expired"),
            DeliveryDecision::Reject
        );
    }

    #[test]
    fn message_id_from_headers() {
        assert_eq!(
            message_id(b"Subject: x\r\nMessage-ID: <a@b>\r\n\r\nbody").as_deref(),
            Some("a@b")
        );
        assert_eq!(message_id(b"Subject: x\r\n\r\nbody"), None);
    }
}
