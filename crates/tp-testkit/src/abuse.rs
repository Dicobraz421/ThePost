//! In-memory блокировки источников и учётных записей (CONTRACTS.md#AbuseBlocks; решение 2026-11-01).
//!
//! [`MemAbuseBlocks`] — блокировки по организациям со сроком по [`Clock`]; с шиной
//! ([`MemAbuseBlocks::with_bus`]) `block` публикует сигнал `Blocked`, как реализация `tp-meta`.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::abuse::{AbuseBlock, AbuseBlocks, BlockSubject};
use tp_contracts::bus::{Event, EventBus, EventKind};
use tp_contracts::{Error, Result, TenantId};

use crate::infra::{Clock, SystemClock, lock};

#[derive(Debug, Default)]
struct State {
    blocks: HashMap<(TenantId, BlockSubject), AbuseBlock>,
    faults: VecDeque<Error>,
}

/// Блокировки в памяти; клоны разделяют состояние.
#[derive(Clone)]
pub struct MemAbuseBlocks {
    state: Arc<Mutex<State>>,
    clock: Arc<dyn Clock>,
    bus: Option<Arc<dyn EventBus>>,
}

impl fmt::Debug for MemAbuseBlocks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemAbuseBlocks")
            .field("state", &self.state)
            .field("bus", &self.bus.is_some())
            .finish()
    }
}

impl MemAbuseBlocks {
    /// Пустое хранилище с часами `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            clock,
            bus: None,
        }
    }

    /// Публиковать `Blocked` в `bus` после `block`.
    pub fn with_bus(mut self, bus: Arc<dyn EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Следующая операция вернёт `err` (очередь, по одной на операцию).
    pub fn fail_next(&self, err: Error) {
        lock(&self.state).faults.push_back(err);
    }
}

impl Default for MemAbuseBlocks {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

fn fault(st: &mut State) -> Result<()> {
    st.faults.pop_front().map_or(Ok(()), Err)
}

#[async_trait]
impl AbuseBlocks for MemAbuseBlocks {
    async fn block(
        &self,
        tenant: TenantId,
        subject: &BlockSubject,
        until: OffsetDateTime,
        reason: &str,
    ) -> Result<()> {
        let now = self.clock.now();
        {
            let mut st = lock(&self.state);
            fault(&mut st)?;
            if until <= now {
                return Err(Error::invalid("block until must be in the future"));
            }
            let entry = st
                .blocks
                .entry((tenant, *subject))
                .or_insert_with(|| AbuseBlock {
                    subject: *subject,
                    until,
                    reason: String::new(),
                });
            // Истёкшая блокировка начинается заново; действующая не сокращается.
            entry.until = if entry.until > now {
                entry.until.max(until)
            } else {
                until
            };
            reason.clone_into(&mut entry.reason);
        }
        if let Some(bus) = &self.bus {
            // Сигнал после «фиксации»; потеря сигнала не отменяет блокировку.
            let _ = bus
                .publish(Event {
                    tenant,
                    account: None,
                    kind: EventKind::Blocked { subject: *subject },
                })
                .await;
        }
        Ok(())
    }

    async fn blocked(
        &self,
        tenant: TenantId,
        subject: &BlockSubject,
    ) -> Result<Option<OffsetDateTime>> {
        let now = self.clock.now();
        let mut st = lock(&self.state);
        fault(&mut st)?;
        Ok(st
            .blocks
            .iter()
            .filter(|((t, s), b)| *t == tenant && s.covers(subject) && b.until > now)
            .map(|(_, b)| b.until)
            .max())
    }

    async fn unblock(&self, tenant: TenantId, subject: &BlockSubject) -> Result<()> {
        let mut st = lock(&self.state);
        fault(&mut st)?;
        st.blocks.remove(&(tenant, *subject));
        Ok(())
    }

    async fn active(&self, tenant: TenantId) -> Result<Vec<AbuseBlock>> {
        let now = self.clock.now();
        let mut st = lock(&self.state);
        fault(&mut st)?;
        st.blocks.retain(|_, b| b.until > now);
        Ok(st
            .blocks
            .iter()
            .filter(|((t, _), _)| *t == tenant)
            .map(|(_, b)| b.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;
    use tp_contracts::AccountId;

    use super::*;
    use crate::{ManualClock, MemEventBus, suite};

    #[tokio::test]
    async fn passes_contract_suite() {
        let clock = ManualClock::default();
        let blocks = MemAbuseBlocks::new(clock.shared());
        suite::abuse_blocks(&blocks, clock.now(), Duration::from_secs(60), &clock).await;
    }

    #[tokio::test]
    async fn publishes_blocked_and_faults() {
        let clock = ManualClock::default();
        let bus = MemEventBus::default();
        let blocks = MemAbuseBlocks::new(clock.shared()).with_bus(Arc::new(bus.clone()));
        let mut sys = bus.subscribe_system().await.expect("subscribe");
        let tenant = TenantId::new();
        let subject = BlockSubject::Account(AccountId::new());
        let until = clock.now() + Duration::from_secs(60);
        blocks
            .block(tenant, &subject, until, "imap: 10 failed logins")
            .await
            .expect("block");
        assert_eq!(
            sys.next().await,
            Some(Event {
                tenant,
                account: None,
                kind: EventKind::Blocked { subject },
            })
        );
        blocks.fail_next(Error::UNAVAILABLE);
        assert_eq!(
            blocks.blocked(tenant, &subject).await,
            Err(Error::UNAVAILABLE)
        );
        assert_eq!(blocks.blocked(tenant, &subject).await, Ok(Some(until)));
    }
}
