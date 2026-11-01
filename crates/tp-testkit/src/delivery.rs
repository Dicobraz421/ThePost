//! In-memory журнал доставки (CONTRACTS.md#DeliveryLog; SRS 5.2).
//!
//! [`MemDeliveryLog`] хранит записи в порядке записи; [`MemDeliveryLog::events`] — для проверок
//! в тестах полос (все события письма записаны).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::delivery::{
    DeliveryCursor, DeliveryEvent, DeliveryLog, DeliveryPage, DeliveryQuery,
};
use tp_contracts::{Error, Result, TenantId};

use crate::infra::lock;

#[derive(Debug, Default)]
struct State {
    /// Номер записи (порядок записи) и запись.
    events: Vec<(u64, DeliveryEvent)>,
    next_seq: u64,
    faults: VecDeque<Error>,
}

/// Журнал доставки в памяти; клоны разделяют записи.
#[derive(Debug, Clone, Default)]
pub struct MemDeliveryLog {
    state: Arc<Mutex<State>>,
}

impl MemDeliveryLog {
    /// Пустой журнал.
    pub fn new() -> Self {
        Self::default()
    }

    /// Все записи в порядке записи (по всем организациям).
    pub fn events(&self) -> Vec<DeliveryEvent> {
        lock(&self.state)
            .events
            .iter()
            .map(|(_, e)| e.clone())
            .collect()
    }

    /// Следующая операция (`record`, `record_batch`, `search` или `purge`) вернёт `err`
    /// (очередь, по одной на операцию).
    pub fn fail_next(&self, err: Error) {
        lock(&self.state).faults.push_back(err);
    }
}

fn fault(st: &mut State) -> Result<()> {
    st.faults.pop_front().map_or(Ok(()), Err)
}

/// Позиция в порядке поиска: (момент в наносекундах, номер записи).
fn position(at: OffsetDateTime, seq: u64) -> (i128, u64) {
    (at.unix_timestamp_nanos(), seq)
}

fn encode(pos: (i128, u64)) -> DeliveryCursor {
    DeliveryCursor(format!("{}:{}", pos.0, pos.1))
}

fn decode(c: &DeliveryCursor) -> Result<(i128, u64)> {
    let bad = || Error::invalid("invalid delivery cursor");
    let (at, seq) = c.0.split_once(':').ok_or_else(bad)?;
    Ok((
        at.parse().map_err(|_| bad())?,
        seq.parse().map_err(|_| bad())?,
    ))
}

#[async_trait]
impl DeliveryLog for MemDeliveryLog {
    async fn record(&self, event: DeliveryEvent) -> Result<()> {
        self.record_batch(vec![event]).await
    }

    async fn record_batch(&self, events: Vec<DeliveryEvent>) -> Result<()> {
        let mut st = lock(&self.state);
        fault(&mut st)?;
        for e in events {
            let seq = st.next_seq;
            st.next_seq += 1;
            st.events.push((seq, e));
        }
        Ok(())
    }

    async fn search(&self, tenant: TenantId, query: &DeliveryQuery) -> Result<DeliveryPage> {
        query.validate()?;
        let after = query.after.as_ref().map(decode).transpose()?;
        let mut st = lock(&self.state);
        fault(&mut st)?;
        let mut found: Vec<_> = st
            .events
            .iter()
            .filter(|(_, e)| e.tenant == tenant && query.matches(e))
            .map(|(seq, e)| (position(e.at, *seq), e))
            .filter(|(pos, _)| after.is_none_or(|a| *pos > a))
            .collect();
        found.sort_by_key(|(pos, _)| *pos);
        let more = found.len() > query.limit;
        found.truncate(query.limit);
        Ok(DeliveryPage {
            next: more
                .then(|| found.last().map(|(pos, _)| encode(*pos)))
                .flatten(),
            events: found.into_iter().map(|(_, e)| e.clone()).collect(),
        })
    }

    async fn purge(&self, before: OffsetDateTime) -> Result<()> {
        let mut st = lock(&self.state);
        fault(&mut st)?;
        st.events.retain(|(_, e)| e.at >= before);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::{DeliveryFixture, delivery_log};
    use tp_contracts::delivery::{DeliveryDecision, DeliveryEventKind};
    use tp_contracts::membership::NodeId;

    #[tokio::test]
    async fn passes_contract_suite() {
        delivery_log(
            &MemDeliveryLog::new(),
            &DeliveryFixture {
                tenant: TenantId::DEFAULT,
                foreign_tenant: TenantId::new(),
            },
        )
        .await;
    }

    fn event(at: i64) -> DeliveryEvent {
        DeliveryEvent {
            tenant: TenantId::DEFAULT,
            at: OffsetDateTime::from_unix_timestamp(at).expect("time"),
            node: NodeId::new("n1").expect("node"),
            kind: DeliveryEventKind::Deliver,
            sender: "a@example.org".into(),
            recipients: vec!["b@example.org".into()],
            message_id: None,
            queue_id: None,
            decision: DeliveryDecision::Accept,
            reason: None,
        }
    }

    #[tokio::test]
    async fn purge_faults_and_cursor() {
        let log = MemDeliveryLog::new();
        log.record_batch(vec![event(10), event(20), event(30)])
            .await
            .expect("record");
        log.purge(OffsetDateTime::from_unix_timestamp(20).expect("time"))
            .await
            .expect("purge");
        assert_eq!(log.events(), vec![event(20), event(30)]);

        log.fail_next(Error::UNAVAILABLE);
        assert_eq!(log.record(event(40)).await, Err(Error::UNAVAILABLE));
        assert_eq!(log.events().len(), 2, "сбой — без записи");

        let mut q = DeliveryQuery::all(1);
        q.after = Some(DeliveryCursor("garbage".into()));
        assert!(matches!(
            log.search(TenantId::DEFAULT, &q).await,
            Err(Error::Invalid(_))
        ));
    }
}
