//! In-memory членство узлов (CONTRACTS.md#Membership; решение 2026-10-30, #45).
//!
//! [`MemMembership`] — записи узлов со сроком по [`Clock`]: с [`crate::ManualClock`] тест двигает время
//! и проверяет продление, истечение и выход узла без ожидания.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::membership::{Membership, NodeId};
use tp_contracts::{Error, Result};

use crate::infra::{Clock, SystemClock, add, lock};

#[derive(Debug, Default)]
struct State {
    /// Узел → момент истечения записи.
    nodes: BTreeMap<NodeId, OffsetDateTime>,
    faults: VecDeque<Error>,
}

/// Членство узлов в памяти; клоны разделяют состав. Время — [`Clock`] (у PostgreSQL — часы СУБД).
#[derive(Debug, Clone)]
pub struct MemMembership {
    state: Arc<Mutex<State>>,
    clock: Arc<dyn Clock>,
}

impl MemMembership {
    /// Пустой состав с часами `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            clock,
        }
    }

    /// Момент истечения записи узла, если запись есть (в том числе просроченная).
    pub fn expires_at(&self, node: &NodeId) -> Option<OffsetDateTime> {
        lock(&self.state).nodes.get(node).copied()
    }

    /// Следующая операция (`heartbeat`, `leave` или `live`) вернёт `err` (очередь, по одной на операцию).
    pub fn fail_next(&self, err: Error) {
        lock(&self.state).faults.push_back(err);
    }

    /// Снимает инъецированные ошибки.
    pub fn clear_faults(&self) {
        lock(&self.state).faults.clear();
    }
}

impl Default for MemMembership {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

fn fault(st: &mut State) -> Result<()> {
    match st.faults.pop_front() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[async_trait]
impl Membership for MemMembership {
    async fn heartbeat(&self, node: &NodeId, ttl: Duration) -> Result<()> {
        let expires_at = add(self.clock.now(), ttl);
        let mut st = lock(&self.state);
        fault(&mut st)?;
        st.nodes.insert(node.clone(), expires_at);
        Ok(())
    }

    async fn leave(&self, node: &NodeId) -> Result<()> {
        let mut st = lock(&self.state);
        fault(&mut st)?;
        st.nodes.remove(node);
        Ok(())
    }

    async fn live(&self) -> Result<Vec<NodeId>> {
        let now = self.clock.now();
        let mut st = lock(&self.state);
        fault(&mut st)?;
        // Просроченные записи удаляются, как при очистке таблицы в PostgreSQL; на ответ это не влияет.
        st.nodes.retain(|_, expires_at| *expires_at > now);
        Ok(st.nodes.keys().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManualClock;
    use crate::suite::membership;

    const TTL: Duration = Duration::from_secs(30);

    fn node(name: &str) -> NodeId {
        NodeId::new(name).expect("node")
    }

    #[tokio::test]
    async fn passes_contract_suite() {
        let clock = ManualClock::default();
        membership(&MemMembership::new(clock.shared()), TTL, &clock).await;
    }

    #[tokio::test]
    async fn heartbeat_renew_expire_leave() {
        let clock = ManualClock::default();
        let m = MemMembership::new(clock.shared());
        let (a, b) = (node("mailbox-a"), node("mailbox-b"));
        m.heartbeat(&b, TTL).await.expect("hb");
        m.heartbeat(&a, TTL).await.expect("hb");
        assert_eq!(m.expires_at(&a), Some(clock.now() + TTL));
        assert_eq!(m.live().await.expect("live"), vec![a.clone(), b.clone()]);

        // Продление считает срок от текущего момента; граница срока — уже не жив.
        clock.advance(TTL / 3);
        m.heartbeat(&a, TTL).await.expect("hb");
        clock.advance(TTL - TTL / 3 - Duration::from_secs(1));
        assert_eq!(m.live().await.expect("live"), vec![a.clone(), b.clone()]);
        clock.advance(Duration::from_secs(1));
        assert_eq!(m.live().await.expect("live"), vec![a.clone()]);
        assert_eq!(m.expires_at(&b), None);

        // Более короткий срок сокращает запись.
        m.heartbeat(&a, Duration::ZERO).await.expect("hb");
        assert!(m.live().await.expect("live").is_empty());

        // Просроченный узел возвращается продлением; выход — сразу, повторный выход — не ошибка.
        m.heartbeat(&b, TTL).await.expect("hb");
        assert_eq!(m.live().await.expect("live"), vec![b.clone()]);
        m.leave(&b).await.expect("leave");
        m.leave(&b).await.expect("leave again");
        assert!(m.live().await.expect("live").is_empty());
    }

    #[tokio::test]
    async fn clones_share_and_faults() {
        let m = MemMembership::default();
        let other = m.clone();
        let a = node("a");
        m.heartbeat(&a, TTL).await.expect("hb");
        assert_eq!(other.live().await.expect("live"), vec![a.clone()]);

        m.fail_next(Error::UNAVAILABLE);
        assert!(m.heartbeat(&a, TTL).await.is_err_and(|e| e.is_retryable()));
        m.fail_next(Error::UNAVAILABLE);
        assert_eq!(m.live().await, Err(Error::UNAVAILABLE));
        m.fail_next(Error::UNAVAILABLE);
        m.clear_faults();
        assert_eq!(m.live().await.expect("live"), vec![a]);
    }
}
