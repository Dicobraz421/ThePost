//! In-memory постановка исходящего письма (CONTRACTS.md#Queue; CTR-28).
//!
//! [`MemOutbound`] запоминает письма (`enqueue` и `submit`) и отвечает на `submit` статусами, заданными
//! [`MemOutbound::set_status`] (остальные получатели — в очереди); [`MemOutbound::fail_next`] — ошибка следующего
//! вызова. Отложенные письма ([`Outbound::schedule`], CTR-42) — [`MemOutbound::scheduled`]; [`MemOutbound::release`]
//! имитирует выпуск (после него отмена — `Conflict`).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use time::OffsetDateTime;
use tp_contracts::queue::{Outbound, SubmitStatus, Submitted};
use tp_contracts::{AccountId, Error, QueueId, Result, TenantId};

use crate::infra::lock;

/// Письмо, принятое [`MemOutbound`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    /// Организация.
    pub tenant: TenantId,
    /// Учётная запись отправителя (CTR-74).
    pub owner: Option<AccountId>,
    /// Отправитель конверта.
    pub mail_from: String,
    /// Получатели в порядке вызова.
    pub rcpts: Vec<String>,
    /// Письмо.
    pub raw: Bytes,
}

/// Отложенное письмо [`MemOutbound`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scheduled {
    /// Запись очереди.
    pub queue: QueueId,
    /// Письмо.
    pub message: Queued,
    /// Срок выпуска.
    pub release_at: OffsetDateTime,
    /// Выпущено ([`MemOutbound::release`]).
    pub released: bool,
}

#[derive(Debug, Default)]
struct State {
    queued: Vec<Queued>,
    scheduled: Vec<Scheduled>,
    statuses: HashMap<String, SubmitStatus>,
    faults: VecDeque<Error>,
}

/// [`Outbound`] в памяти; клоны разделяют состояние.
#[derive(Debug, Clone, Default)]
pub struct MemOutbound {
    state: Arc<Mutex<State>>,
}

impl MemOutbound {
    /// Принятые письма в порядке вызовов.
    pub fn queued(&self) -> Vec<Queued> {
        lock(&self.state).queued.clone()
    }

    /// Отложенные письма (не отменённые) в порядке вызовов.
    pub fn scheduled(&self) -> Vec<Scheduled> {
        lock(&self.state).scheduled.clone()
    }

    /// Выпуск отложенного письма: дальше `cancel_scheduled` — `Conflict`. Нет записи — `false`.
    pub fn release(&self, queue: QueueId) -> bool {
        let mut st = lock(&self.state);
        match st.scheduled.iter_mut().find(|s| s.queue == queue) {
            Some(s) => {
                s.released = true;
                true
            }
            None => false,
        }
    }

    /// Статус получателя `rcpt` (без учёта регистра) в ответах `submit`.
    pub fn set_status(&self, rcpt: &str, status: SubmitStatus) {
        lock(&self.state)
            .statuses
            .insert(rcpt.to_lowercase(), status);
    }

    /// Следующий вызов `enqueue` или `submit` вернёт `err`, письмо не запоминается.
    pub fn fail_next(&self, err: Error) {
        lock(&self.state).faults.push_back(err);
    }

    fn accept(
        &self,
        tenant: TenantId,
        owner: Option<AccountId>,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<()> {
        let mut st = lock(&self.state);
        if let Some(e) = st.faults.pop_front() {
            return Err(e);
        }
        st.queued.push(Queued {
            tenant,
            owner,
            mail_from: mail_from.to_owned(),
            rcpts: rcpts.to_vec(),
            raw,
        });
        Ok(())
    }
}

#[async_trait]
impl Outbound for MemOutbound {
    async fn enqueue(
        &self,
        tenant: TenantId,
        owner: Option<AccountId>,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<()> {
        self.accept(tenant, owner, mail_from, rcpts, raw)
    }

    async fn schedule(
        &self,
        tenant: TenantId,
        owner: Option<AccountId>,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
        release_at: OffsetDateTime,
    ) -> Result<QueueId> {
        let mut st = lock(&self.state);
        if let Some(e) = st.faults.pop_front() {
            return Err(e);
        }
        let queue = QueueId::new();
        st.scheduled.push(Scheduled {
            queue,
            message: Queued {
                tenant,
                owner,
                mail_from: mail_from.to_owned(),
                rcpts: rcpts.to_vec(),
                raw,
            },
            release_at,
            released: false,
        });
        Ok(queue)
    }

    async fn cancel_scheduled(&self, tenant: TenantId, queue: QueueId) -> Result<()> {
        let mut st = lock(&self.state);
        if let Some(e) = st.faults.pop_front() {
            return Err(e);
        }
        let pos = st
            .scheduled
            .iter()
            .position(|s| s.queue == queue && s.message.tenant == tenant)
            .ok_or(Error::NotFound)?;
        if st.scheduled[pos].released {
            return Err(Error::Conflict);
        }
        st.scheduled.remove(pos);
        Ok(())
    }

    async fn submit(
        &self,
        tenant: TenantId,
        owner: Option<AccountId>,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<Submitted> {
        self.accept(tenant, owner, mail_from, rcpts, raw)?;
        let st = lock(&self.state);
        Ok(Submitted {
            rcpts: rcpts
                .iter()
                .map(|r| {
                    let s = st
                        .statuses
                        .get(&r.to_lowercase())
                        .cloned()
                        .unwrap_or(SubmitStatus::Queued);
                    (r.clone(), s)
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite;

    #[tokio::test]
    async fn statuses_and_faults() {
        let ob = MemOutbound::default();
        suite::outbound_submit(&ob, TenantId::DEFAULT, &["a@example.com".into()]).await;
        ob.set_status("B@example.org", SubmitStatus::Delivered);
        let rcpts = vec!["b@example.org".to_owned(), "c@example.net".to_owned()];
        let owner = AccountId::new();
        let sent = ob
            .submit(
                TenantId::DEFAULT,
                Some(owner),
                "x@example.org",
                &rcpts,
                Bytes::from_static(b"m"),
            )
            .await
            .expect("submit");
        assert_eq!(
            sent.rcpts,
            vec![
                ("b@example.org".to_owned(), SubmitStatus::Delivered),
                ("c@example.net".to_owned(), SubmitStatus::Queued),
            ]
        );
        ob.fail_next(Error::UNAVAILABLE);
        assert!(
            ob.enqueue(
                TenantId::DEFAULT,
                None,
                "x@example.org",
                &rcpts,
                Bytes::new()
            )
            .await
            .is_err()
        );
        assert_eq!(ob.queued().len(), 3);
        assert_eq!(ob.queued()[2].rcpts, rcpts);
        assert_eq!(ob.queued()[2].owner, Some(owner));
    }

    /// CTR-42: отложенная отправка, отмена до выпуска, после выпуска — `Conflict`.
    #[tokio::test]
    async fn schedule_and_cancel() {
        let ob = MemOutbound::default();
        let t = TenantId::DEFAULT;
        let rcpts = vec!["b@example.org".to_owned()];
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(1);
        suite::outbound_schedule(&ob, t, &rcpts, at).await;
        assert!(ob.scheduled().is_empty());
        let owner = AccountId::new();
        let id = ob
            .schedule(t, Some(owner), "x@example.org", &rcpts, Bytes::new(), at)
            .await
            .expect("schedule");
        assert_eq!(ob.scheduled()[0].release_at, at);
        assert_eq!(ob.scheduled()[0].message.owner, Some(owner));
        assert!(ob.release(id));
        assert_eq!(ob.cancel_scheduled(t, id).await, Err(Error::Conflict));
        assert!(ob.queued().is_empty());
    }
}
