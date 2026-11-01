//! Журнал доставки по модели message tracking Exchange (CONTRACTS.md#DeliveryLog; SRS 5.2,
//! решение 2026-11-01-borrowings-security).
//!
//! Для каждого письма — отправитель, получатели, Message-ID, событие, решение и его причина, узел, время;
//! содержимое писем не хранится. Пишет `tp-smtp` (SMTP-17), ищет Admin API; срок хранения — секция конфигурации
//! реализации (по умолчанию 30 суток), удаление — [`DeliveryLog::purge`] из роли maintenance.
//! Реализация — `tp-meta` (META-22, только INSERT и SELECT для роли приложения), in-memory — `tp-testkit`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::ids::{QueueId, TenantId};
use crate::membership::NodeId;

/// Событие журнала доставки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryEventKind {
    /// Приём письма (smtp-in, submission).
    Receive,
    /// Отказ в приёме или доставке.
    Reject,
    /// Доставка в ящики организации.
    Deliver,
    /// Передача наружу (smtp-out, внешний MTA).
    Relay,
    /// Отправка уведомления о доставке (DSN).
    Dsn,
}

/// Решение по письму в событии.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryDecision {
    /// Принято, доставлено или передано.
    Accept,
    /// Постоянный отказ (5xx).
    Reject,
    /// Временный отказ или отложено (4xx, повтор из очереди).
    Defer,
    /// Помещено в карантин.
    Quarantine,
    /// Принято и отброшено без доставки.
    Discard,
    /// Задержано до решения модератора группы рассылки.
    Hold,
}

/// Запись журнала доставки. Адреса — как в конверте SMTP; содержимое письма не входит.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryEvent {
    /// Организация.
    pub tenant: TenantId,
    /// Момент события (UTC).
    pub at: OffsetDateTime,
    /// Узел, на котором произошло событие.
    pub node: NodeId,
    /// Событие.
    pub kind: DeliveryEventKind,
    /// Отправитель конверта (`MAIL FROM`); пустой — уведомление о недоставке.
    pub sender: String,
    /// Получатели события (`RCPT TO`); пусто — отказ до `RCPT`.
    pub recipients: Vec<String>,
    /// Заголовок Message-ID без угловых скобок; `None` — нет или ещё не известен (отказ до `DATA`).
    pub message_id: Option<String>,
    /// Запись очереди исходящей почты, если событие из очереди.
    pub queue_id: Option<QueueId>,
    /// Решение.
    pub decision: DeliveryDecision,
    /// Причина решения: ответ SMTP, вердикт проверки, правило; `None` — без пояснения.
    pub reason: Option<String>,
}

/// Позиция продолжения поиска; непрозрачна, действительна для того же запроса.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeliveryCursor(pub String);

/// Запрос к журналу доставки. Условия объединяются по «и»; `None` — без условия.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryQuery {
    /// Отправитель конверта (точное совпадение без учёта регистра ASCII).
    pub sender: Option<String>,
    /// Один из получателей (точное совпадение без учёта регистра ASCII).
    pub recipient: Option<String>,
    /// Message-ID (точное совпадение).
    pub message_id: Option<String>,
    /// Начало интервала, включительно.
    pub since: Option<OffsetDateTime>,
    /// Конец интервала, не включительно.
    pub until: Option<OffsetDateTime>,
    /// Не более записей на страницу, ≥ 1 (иначе `Error::Invalid`).
    pub limit: usize,
    /// Продолжение после страницы с этой позицией.
    pub after: Option<DeliveryCursor>,
}

impl DeliveryQuery {
    /// Запрос без условий на `limit` записей.
    pub fn all(limit: usize) -> Self {
        DeliveryQuery {
            sender: None,
            recipient: None,
            message_id: None,
            since: None,
            until: None,
            limit,
            after: None,
        }
    }

    /// Проверка параметров: `limit` ≥ 1, `since < until`; иначе `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.limit == 0 {
            return Err(Error::invalid("delivery query limit must be positive"));
        }
        if let (Some(since), Some(until)) = (self.since, self.until)
            && since >= until
        {
            return Err(Error::invalid("delivery query interval is empty"));
        }
        Ok(())
    }

    /// Запись удовлетворяет условиям запроса (кроме организации и позиции).
    pub fn matches(&self, e: &DeliveryEvent) -> bool {
        self.sender
            .as_deref()
            .is_none_or(|s| s.eq_ignore_ascii_case(&e.sender))
            && self
                .recipient
                .as_deref()
                .is_none_or(|r| e.recipients.iter().any(|x| x.eq_ignore_ascii_case(r)))
            && self
                .message_id
                .as_deref()
                .is_none_or(|m| e.message_id.as_deref() == Some(m))
            && self.since.is_none_or(|t| e.at >= t)
            && self.until.is_none_or(|t| e.at < t)
    }
}

/// Страница результатов поиска.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryPage {
    /// Записи по возрастанию `at`; равные моменты — в порядке записи.
    pub events: Vec<DeliveryEvent>,
    /// Позиция следующей страницы; `None` — записей больше нет.
    pub next: Option<DeliveryCursor>,
}

/// Журнал доставки: только добавление и поиск; изменение записей не предусмотрено.
#[async_trait]
pub trait DeliveryLog: Send + Sync + 'static {
    /// Добавляет запись (организация — `event.tenant`).
    async fn record(&self, event: DeliveryEvent) -> Result<()>;

    /// Добавляет записи пачкой; по умолчанию — по одной через [`DeliveryLog::record`].
    async fn record_batch(&self, events: Vec<DeliveryEvent>) -> Result<()> {
        for event in events {
            self.record(event).await?;
        }
        Ok(())
    }

    /// Поиск в организации `tenant` ([`DeliveryQuery::matches`]); некорректный запрос — `Error::Invalid`,
    /// чужая или устаревшая позиция — `Error::Invalid`.
    async fn search(&self, tenant: TenantId, query: &DeliveryQuery) -> Result<DeliveryPage>;

    /// Удаляет записи старше `before` по всем организациям (роль maintenance). Реализация может удалять
    /// целыми сутками (секции): записи суток, на которые приходится `before`, могут остаться.
    async fn purge(&self, before: OffsetDateTime) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_safe() {
        fn _log(_: std::sync::Arc<dyn DeliveryLog>) {}
    }

    fn event() -> DeliveryEvent {
        DeliveryEvent {
            tenant: TenantId::DEFAULT,
            at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
            node: NodeId::new("edge-1").expect("node"),
            kind: DeliveryEventKind::Receive,
            sender: "Alice@Example.org".into(),
            recipients: vec!["bob@example.org".into(), "carol@example.org".into()],
            message_id: Some("abc@example.org".into()),
            queue_id: None,
            decision: DeliveryDecision::Accept,
            reason: None,
        }
    }

    #[test]
    fn query_matches() {
        let e = event();
        let at = e.at;
        let q = DeliveryQuery::all(10);
        assert!(q.matches(&e));
        let with = |f: fn(&mut DeliveryQuery)| {
            let mut q = DeliveryQuery::all(10);
            f(&mut q);
            q
        };
        assert!(with(|q| q.sender = Some("alice@example.ORG".into())).matches(&e));
        assert!(!with(|q| q.sender = Some("bob@example.org".into())).matches(&e));
        assert!(with(|q| q.recipient = Some("CAROL@example.org".into())).matches(&e));
        assert!(!with(|q| q.recipient = Some("dave@example.org".into())).matches(&e));
        assert!(with(|q| q.message_id = Some("abc@example.org".into())).matches(&e));
        assert!(!with(|q| q.message_id = Some("ABC@example.org".into())).matches(&e));
        let mut q = DeliveryQuery::all(10);
        q.since = Some(at);
        q.until = Some(at + time::Duration::SECOND);
        assert!(q.matches(&e), "since включительно");
        q.until = Some(at);
        q.since = None;
        assert!(!q.matches(&e), "until не включительно");
    }

    #[test]
    fn query_validate() {
        assert!(DeliveryQuery::all(1).validate().is_ok());
        assert!(matches!(
            DeliveryQuery::all(0).validate(),
            Err(Error::Invalid(_))
        ));
        let mut q = DeliveryQuery::all(1);
        q.since = Some(OffsetDateTime::UNIX_EPOCH);
        q.until = Some(OffsetDateTime::UNIX_EPOCH);
        assert!(matches!(q.validate(), Err(Error::Invalid(_))));
    }

    #[test]
    fn serde_strings() {
        let json = serde_json::to_value(event()).expect("ser");
        assert_eq!(json["kind"], "receive");
        assert_eq!(json["decision"], "accept");
        assert_eq!(json["node"], "edge-1");
        let back: DeliveryEvent = serde_json::from_value(json).expect("de");
        assert_eq!(back, event());
    }
}
