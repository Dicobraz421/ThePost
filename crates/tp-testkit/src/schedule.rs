//! Заглушки планирования встреч (CONTRACTS.md#Schedule, CTR-47): [`FakeItip`] — приём iTIP для тестов `tp-smtp`
//! (SMTP-33) и `tp-jmap`, [`MemItipSender`] — отправка iMIP без очереди. Занятость и входящие планирования в памяти —
//! [`crate::MemDav`].

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::queue::{SubmitStatus, Submitted};
use tp_contracts::schedule::{ItipDelivery, ItipMessage, ItipOutcome, ItipOutgoing, ItipSender};
use tp_contracts::{Error, Result};

use crate::infra::lock;

#[derive(Debug, Default)]
struct FakeState {
    outcomes: HashMap<String, ItipOutcome>,
    faults: VecDeque<Error>,
    calls: Vec<ItipMessage>,
}

/// Заглушка приёма iTIP: итог по адресу получателя ([`FakeItip::set`]), по умолчанию — `Applied`; недопустимое
/// сообщение ([`ItipMessage::validate`]) — `Ignored`; [`FakeItip::fail_next`] — ошибка следующего вызова;
/// [`FakeItip::calls`] — журнал вызовов.
#[derive(Debug, Clone, Default)]
pub struct FakeItip {
    state: Arc<Mutex<FakeState>>,
}

impl FakeItip {
    /// Всё применяется.
    pub fn new() -> Self {
        Self::default()
    }

    /// Итог `outcome` для получателя `to` ([`ItipMessage::to`]).
    pub fn set(&self, to: impl Into<String>, outcome: ItipOutcome) {
        lock(&self.state).outcomes.insert(to.into(), outcome);
    }

    /// Следующий вызов вернёт `err` (очередь, по одной на вызов).
    pub fn fail_next(&self, err: Error) {
        lock(&self.state).faults.push_back(err);
    }

    /// Вызовы по порядку.
    pub fn calls(&self) -> Vec<ItipMessage> {
        lock(&self.state).calls.clone()
    }
}

#[async_trait]
impl ItipDelivery for FakeItip {
    async fn deliver(&self, msg: &ItipMessage) -> Result<ItipOutcome> {
        let mut st = lock(&self.state);
        st.calls.push(msg.clone());
        if let Some(e) = st.faults.pop_front() {
            return Err(e);
        }
        if let Err(e) = msg.validate() {
            return Ok(ItipOutcome::Ignored(e.to_string()));
        }
        Ok(st
            .outcomes
            .get(&msg.to)
            .cloned()
            .unwrap_or(ItipOutcome::Applied))
    }
}

#[derive(Debug, Default)]
struct SenderState {
    faults: VecDeque<Error>,
    sent: Vec<ItipOutgoing>,
}

/// Отправка iMIP в памяти: проверяет письмо ([`ItipOutgoing::validate`]), запоминает его ([`MemItipSender::sent`])
/// и отвечает `Queued` для всех получателей; [`MemItipSender::fail_next`] — ошибка следующего вызова.
#[derive(Debug, Clone, Default)]
pub struct MemItipSender {
    state: Arc<Mutex<SenderState>>,
}

impl MemItipSender {
    /// Без отправленных писем.
    pub fn new() -> Self {
        Self::default()
    }

    /// Следующий вызов вернёт `err` (очередь, по одной на вызов).
    pub fn fail_next(&self, err: Error) {
        lock(&self.state).faults.push_back(err);
    }

    /// Отправленные письма по порядку.
    pub fn sent(&self) -> Vec<ItipOutgoing> {
        lock(&self.state).sent.clone()
    }
}

#[async_trait]
impl ItipSender for MemItipSender {
    async fn send(&self, msg: &ItipOutgoing) -> Result<Submitted> {
        msg.validate()?;
        let mut st = lock(&self.state);
        if let Some(e) = st.faults.pop_front() {
            return Err(e);
        }
        st.sent.push(msg.clone());
        Ok(Submitted::all(&msg.to, SubmitStatus::Queued))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_contracts::meta::AccountScope;
    use tp_contracts::schedule::{ItipMethod, ItipSource};
    use tp_contracts::{AccountId, TenantId};

    fn scope() -> AccountScope {
        AccountScope {
            tenant: TenantId::new(),
            account: AccountId::new(),
        }
    }

    fn msg(to: &str) -> ItipMessage {
        ItipMessage {
            recipient: scope(),
            to: to.into(),
            from: "boss@partner.example".into(),
            method: ItipMethod::Request,
            ics: "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n".into(),
            source: ItipSource::Imip,
        }
    }

    #[tokio::test]
    async fn fake_itip() {
        let fake = FakeItip::new();
        fake.set("bob@example.org", ItipOutcome::Ignored("подделан".into()));
        assert_eq!(
            fake.deliver(&msg("anna@example.org")).await,
            Ok(ItipOutcome::Applied)
        );
        assert_eq!(
            fake.deliver(&msg("bob@example.org")).await,
            Ok(ItipOutcome::Ignored("подделан".into()))
        );
        let empty = ItipMessage {
            ics: String::new(),
            ..msg("anna@example.org")
        };
        assert!(matches!(
            fake.deliver(&empty).await,
            Ok(ItipOutcome::Ignored(_))
        ));
        fake.fail_next(Error::UNAVAILABLE);
        assert_eq!(
            fake.deliver(&msg("anna@example.org")).await,
            Err(Error::UNAVAILABLE)
        );
        assert_eq!(fake.calls().len(), 4);
        assert_eq!(fake.calls()[1].to, "bob@example.org");
    }

    #[tokio::test]
    async fn mem_sender() {
        let sender = MemItipSender::new();
        let out = ItipOutgoing {
            sender: scope(),
            from: "anna@example.org".into(),
            to: vec!["a@partner.example".into(), "b@partner.example".into()],
            method: ItipMethod::Request,
            ics: "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n".into(),
            subject: "Приглашение: планёрка".into(),
            text: "Планёрка в 10:00".into(),
        };
        let sub = sender.send(&out).await.expect("send");
        assert_eq!(sub, Submitted::all(&out.to, SubmitStatus::Queued));
        assert!(matches!(
            sender
                .send(&ItipOutgoing {
                    to: Vec::new(),
                    ..out.clone()
                })
                .await,
            Err(Error::Invalid(_))
        ));
        sender.fail_next(Error::UNAVAILABLE);
        assert_eq!(sender.send(&out).await, Err(Error::UNAVAILABLE));
        assert_eq!(sender.sent(), vec![out]);
    }
}
