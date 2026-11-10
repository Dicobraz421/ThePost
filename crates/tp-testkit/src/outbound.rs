//! In-memory постановка исходящего письма (CONTRACTS.md#Queue; CTR-28).
//!
//! [`MemOutbound`] запоминает письма (`enqueue` и `submit`) и отвечает на `submit` статусами, заданными
//! [`MemOutbound::set_status`] (остальные получатели — в очереди); [`MemOutbound::fail_next`] — ошибка следующего
//! вызова.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use tp_contracts::queue::{Outbound, SubmitStatus, Submitted};
use tp_contracts::{Error, Result, TenantId};

use crate::infra::lock;

/// Письмо, принятое [`MemOutbound`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    /// Организация.
    pub tenant: TenantId,
    /// Отправитель конверта.
    pub mail_from: String,
    /// Получатели в порядке вызова.
    pub rcpts: Vec<String>,
    /// Письмо.
    pub raw: Bytes,
}

#[derive(Debug, Default)]
struct State {
    queued: Vec<Queued>,
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
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<()> {
        self.accept(tenant, mail_from, rcpts, raw)
    }

    async fn submit(
        &self,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<Submitted> {
        self.accept(tenant, mail_from, rcpts, raw)?;
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
        let sent = ob
            .submit(
                TenantId::DEFAULT,
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
            ob.enqueue(TenantId::DEFAULT, "x@example.org", &rcpts, Bytes::new())
                .await
                .is_err()
        );
        assert_eq!(ob.queued().len(), 3);
        assert_eq!(ob.queued()[2].rcpts, rcpts);
    }
}
