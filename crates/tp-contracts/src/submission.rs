//! Учёт отправок (JMAP EmailSubmission, RFC 8621 §7; CTR-30; CONTRACTS.md#Identity и EmailSubmission).
//!
//! Запись отправки: письмо, адрес отправителя ([`crate::identity::Identity`]), конверт, момент отправки, состояние
//! отмены (`undoStatus`) и статусы получателей (`deliveryStatus`). Сразу отправленное письмо записывается с
//! [`UndoStatus::Final`].
//!
//! # Отложенная отправка и отмена (CTR-42, решение владельца #195)
//!
//! Отправка с задержкой (`sendAt` в будущем, не дальше `maxDelayedSend` из конфигурации `tp-jmap`) ставит письмо в
//! очередь сразу со сроком выпуска ([`crate::queue::Outbound::schedule`]) и записывается [`UndoStatus::Pending`] с
//! `queue`. Отмена (`undoStatus: canceled`) — сначала [`crate::queue::Outbound::cancel_scheduled`] (письмо уже выпущено —
//! `Conflict`, JMAP `cannotUnsend`), затем [`SubmissionStore::cancel`]. Наступившие отправки ([`SubmissionStore::due`])
//! фиксирует [`SubmissionStore::finalize`]; статусы доставки — журнал доставки по записи очереди
//! (`DeliveryLog::by_queue`). Выборка — [`SubmissionStore::query_submissions`] ([`SubmissionFilter`], RFC 8621 §7.3),
//! изменения для `EmailSubmission/changes` и `queryChanges` — [`SubmissionStore::submission_changes`], срок хранения
//! завершённых и отменённых — [`SubmissionStore::expire_submissions`] (роль maintenance).

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

/// Условие выборки отправок (JMAP `EmailSubmission/query`, FilterCondition RFC 8621 §7.3); `None` — без условия,
/// условия объединяются по «и». `threadIds` вызывающий переводит в `email_ids` (письма цепочки).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SubmissionFilter {
    /// Адрес отправителя — один из (`identityIds`).
    pub identity_ids: Option<Vec<String>>,
    /// Письмо — одно из (`emailIds`, `threadIds`).
    pub email_ids: Option<Vec<MessageId>>,
    /// Состояние отмены (`undoStatus`).
    pub undo_status: Option<UndoStatus>,
    /// `send_at` раньше (`before`, не включительно).
    pub before: Option<OffsetDateTime>,
    /// `send_at` не раньше (`after`, включительно).
    pub after: Option<OffsetDateTime>,
}

impl SubmissionFilter {
    /// Отправка удовлетворяет условиям.
    pub fn matches(&self, s: &EmailSubmission) -> bool {
        self.identity_ids
            .as_ref()
            .is_none_or(|ids| ids.contains(&s.identity))
            && self
                .email_ids
                .as_ref()
                .is_none_or(|ids| ids.contains(&s.email))
            && self.undo_status.is_none_or(|u| u == s.undo_status)
            && self.before.is_none_or(|t| s.send_at < t)
            && self.after.is_none_or(|t| s.send_at >= t)
    }
}

/// Изменения отправок учётной записи после состояния (JMAP `EmailSubmission/changes`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SubmissionChanges {
    /// Созданные после `since` и существующие (по возрастанию).
    pub created: Vec<String>,
    /// Изменённые, существовавшие в `since` (по возрастанию).
    pub updated: Vec<String>,
    /// Удалённые, существовавшие в `since` (по возрастанию); созданные и удалённые после `since` не входят.
    pub destroyed: Vec<String>,
    /// Состояние, до которого учтены изменения.
    pub new_state: u64,
    /// Учтены не все изменения: повторить с `new_state`.
    pub has_more: bool,
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

    /// Отправки учётной записи, удовлетворяющие `filter`, по `send_at`, затем по `id` (CTR-42; сортировка JMAP и
    /// страницы — у вызывающего). По умолчанию — [`SubmissionStore::list`] с фильтром.
    async fn query_submissions(
        &self,
        scope: AccountScope,
        filter: &SubmissionFilter,
    ) -> Result<Vec<EmailSubmission>> {
        let mut all = self.list(scope).await?;
        all.retain(|s| filter.matches(s));
        Ok(all)
    }

    /// Изменения после состояния `since` (CTR-42): не более `limit` отправок (`limit` ≥ 1, иначе `Error::Invalid`),
    /// остальные — `has_more`. Состояние больше текущего или старше хранимой истории — `Error::Conflict` (JMAP
    /// `cannotCalculateChanges`).
    async fn submission_changes(
        &self,
        scope: AccountScope,
        since: u64,
        limit: usize,
    ) -> Result<SubmissionChanges>;

    /// Срок хранения (CTR-42): удаляет [`UndoStatus::Final`] и [`UndoStatus::Canceled`] с `send_at` раньше `before` во
    /// всех организациях, не более `limit`; состояние учётных записей растёт, удалённые — в `destroyed` изменений.
    /// Возвращает число удалённых.
    async fn expire_submissions(&self, before: OffsetDateTime, limit: usize) -> Result<usize>;
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
    fn filter() {
        let s = submission();
        assert!(SubmissionFilter::default().matches(&s));
        let f = SubmissionFilter {
            identity_ids: Some(vec!["home".into(), "work".into()]),
            email_ids: Some(vec![s.email]),
            undo_status: Some(UndoStatus::Pending),
            before: Some(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1)),
            after: Some(OffsetDateTime::UNIX_EPOCH),
        };
        assert!(f.matches(&s));
        for bad in [
            SubmissionFilter {
                identity_ids: Some(vec!["home".into()]),
                ..SubmissionFilter::default()
            },
            SubmissionFilter {
                email_ids: Some(vec![]),
                ..SubmissionFilter::default()
            },
            SubmissionFilter {
                undo_status: Some(UndoStatus::Final),
                ..SubmissionFilter::default()
            },
            SubmissionFilter {
                before: Some(OffsetDateTime::UNIX_EPOCH),
                ..SubmissionFilter::default()
            },
        ] {
            assert!(!bad.matches(&s), "{bad:?}");
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
