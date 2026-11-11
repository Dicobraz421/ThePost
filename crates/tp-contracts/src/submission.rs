//! Учёт отправок (JMAP EmailSubmission, RFC 8621 §7; CTR-30; CONTRACTS.md#Identity и EmailSubmission).
//!
//! Запись отправки: письмо, адрес отправителя ([`crate::identity::Identity`]), конверт, момент отправки, состояние
//! отмены (`undoStatus`) и статусы получателей (`deliveryStatus`). Отправка с задержкой (`sendAt` в будущем) —
//! [`UndoStatus::Pending`]: до момента отправки её можно отменить ([`SubmissionStore::cancel`]); воркер отложенной
//! отправки берёт наступившие ([`SubmissionStore::due`]), ставит письмо в очередь ([`crate::queue::Outbound`]) и
//! фиксирует [`SubmissionStore::finalize`]. Сразу отправленное письмо записывается с [`UndoStatus::Final`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::identity::valid_address;
use crate::ids::{MessageId, QueueId};
use crate::meta::AccountScope;
use crate::queue::SubmitStatus;

/// Наибольшая длина идентификатора отправки и ссылки на адрес отправителя, байт.
pub const MAX_SUBMISSION_ID_LEN: usize = 64;

/// Состояние отмены отправки (JMAP `undoStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UndoStatus {
    /// Ждёт момента отправки; отмена возможна.
    Pending,
    /// Передана в очередь или доставлена; отменить нельзя.
    Final,
    /// Отменена до отправки.
    Canceled,
}

/// Запись отправки (JMAP EmailSubmission).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailSubmission {
    /// Идентификатор в учётной записи.
    pub id: String,
    /// Адрес отправителя (`Identity::id` или идентификатор адреса входа, который выдаёт `tp-jmap`).
    pub identity: String,
    /// Отправляемое письмо.
    pub email: MessageId,
    /// Отправитель конверта (`MAIL FROM`); пустой — уведомление о недоставке.
    pub mail_from: String,
    /// Получатели конверта, в порядке запроса.
    pub rcpt_to: Vec<String>,
    /// Момент отправки (UTC): сейчас или отложенный.
    pub send_at: OffsetDateTime,
    /// Состояние отмены.
    pub undo_status: UndoStatus,
    /// Статусы получателей; до отправки — пусто.
    pub delivery: Vec<(String, SubmitStatus)>,
    /// Запись очереди исходящей почты, если письмо в неё поставлено.
    pub queue: Option<QueueId>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SUBMISSION_ID_LEN
        && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}

impl EmailSubmission {
    /// Проверяет поля: `id` и `identity` — 1–[`MAX_SUBMISSION_ID_LEN`] байт без пробелов и управляющих символов;
    /// `mail_from` — пустой или [`valid_address`]; получатели — хотя бы один, каждый — [`valid_address`]. Иначе —
    /// `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if !valid_id(&self.id) || !valid_id(&self.identity) {
            return Err(Error::invalid("недопустимый идентификатор отправки"));
        }
        if !(self.mail_from.is_empty() || valid_address(&self.mail_from)) {
            return Err(Error::invalid("недопустимый отправитель конверта"));
        }
        if self.rcpt_to.is_empty() || !self.rcpt_to.iter().all(|r| valid_address(r)) {
            return Err(Error::invalid("недопустимые получатели конверта"));
        }
        Ok(())
    }
}

/// Хранилище отправок учётной записи. Реализация — `tp-meta` (карточка meta); in-memory — `tp-testkit`.
#[async_trait]
pub trait SubmissionStore: Send + Sync + 'static {
    /// Записывает отправку. Недопустимые поля ([`EmailSubmission::validate`]) или состояние [`UndoStatus::Canceled`] —
    /// `Error::Invalid`; идентификатор занят в учётной записи — `Error::Conflict`. Возвращает новое состояние.
    async fn insert(&self, scope: AccountScope, submission: EmailSubmission) -> Result<u64>;

    /// Отправка по идентификатору.
    async fn get(&self, scope: AccountScope, id: &str) -> Result<Option<EmailSubmission>>;

    /// Отправки учётной записи по `send_at`, затем по `id`.
    async fn list(&self, scope: AccountScope) -> Result<Vec<EmailSubmission>>;

    /// Отменяет отправку [`UndoStatus::Pending`] и возвращает её. Уже отменённая — не ошибка (возвращается как есть);
    /// [`UndoStatus::Final`] — `Error::Conflict` (JMAP `cannotUnsend`); нет — `Error::NotFound`. Атомарно
    /// с [`SubmissionStore::finalize`]: из отмены и фиксации одной отправки успешна только первая.
    async fn cancel(&self, scope: AccountScope, id: &str) -> Result<EmailSubmission>;

    /// Фиксирует отправку: [`UndoStatus::Final`], статусы получателей `delivery` и запись очереди `queue`.
    /// Повтор для `Final` заменяет статусы и запись очереди; отменённая — `Error::Conflict` (письмо не ставить
    /// в очередь); нет — `Error::NotFound`. Возвращает новое состояние.
    async fn finalize(
        &self,
        scope: AccountScope,
        id: &str,
        delivery: Vec<(String, SubmitStatus)>,
        queue: Option<QueueId>,
    ) -> Result<u64>;

    /// Удаляет запись (JMAP `destroy`; на письмо и очередь не влияет); нет — `Error::NotFound`.
    /// Возвращает новое состояние.
    async fn delete(&self, scope: AccountScope, id: &str) -> Result<u64>;

    /// Наступившие отправки всех организаций: [`UndoStatus::Pending`] с `send_at ≤ now`, по `send_at`, не более
    /// `limit` — для воркера отложенной отправки.
    async fn due(
        &self,
        now: OffsetDateTime,
        limit: usize,
    ) -> Result<Vec<(AccountScope, EmailSubmission)>>;

    /// Состояние отправок учётной записи (JMAP `state`): растёт при каждом изменении, у новой — 0.
    async fn state(&self, scope: AccountScope) -> Result<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn submission() -> EmailSubmission {
        EmailSubmission {
            id: "s1".into(),
            identity: "work".into(),
            email: MessageId::new(),
            mail_from: "anna@example.org".into(),
            rcpt_to: vec!["b@example.com".into()],
            send_at: OffsetDateTime::UNIX_EPOCH,
            undo_status: UndoStatus::Pending,
            delivery: Vec::new(),
            queue: None,
        }
    }

    #[test]
    fn object_safe() {
        fn _store(_: Arc<dyn SubmissionStore>) {}
    }

    #[test]
    fn validate() {
        assert_eq!(submission().validate(), Ok(()));
        let bounce = EmailSubmission {
            mail_from: String::new(),
            ..submission()
        };
        assert_eq!(bounce.validate(), Ok(()));
        let bad = [
            EmailSubmission {
                id: String::new(),
                ..submission()
            },
            EmailSubmission {
                identity: "a b".into(),
                ..submission()
            },
            EmailSubmission {
                mail_from: "anna".into(),
                ..submission()
            },
            EmailSubmission {
                rcpt_to: Vec::new(),
                ..submission()
            },
            EmailSubmission {
                rcpt_to: vec!["x".into()],
                ..submission()
            },
        ];
        for s in bad {
            assert!(s.validate().is_err(), "{s:?}");
        }
    }

    #[test]
    fn serde_roundtrip() {
        let s = EmailSubmission {
            undo_status: UndoStatus::Final,
            delivery: vec![("b@example.com".into(), SubmitStatus::Queued)],
            queue: Some(QueueId::new()),
            ..submission()
        };
        let json = serde_json::to_string(&s).expect("json");
        assert_eq!(
            serde_json::from_str::<EmailSubmission>(&json).expect("parse"),
            s
        );
    }
}
