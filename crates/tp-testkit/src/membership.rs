//! In-memory членство узлов (CONTRACTS.md#Membership; решения 2026-10-30, #45, и 2026-11-01).
//!
//! [`MemMembership`] — записи узлов ([`NodeInfo`]) со сроком по [`Clock`]: с [`crate::ManualClock`] тест двигает
//! время и проверяет продление, истечение и выход узла без ожидания. С шиной ([`MemMembership::with_bus`])
//! вход, выход и изменение записи узла публикуют `MembersChanged`, как реализация `tp-meta`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::bus::{Event, EventBus, EventKind};
use tp_contracts::membership::{Membership, NodeId, NodeInfo};
use tp_contracts::{Error, Result, TenantId};

use crate::infra::{Clock, SystemClock, add, lock};

#[derive(Debug, Default)]
struct State {
    /// Узел → запись и момент её истечения.
    nodes: BTreeMap<NodeId, (NodeInfo, OffsetDateTime)>,
    faults: VecDeque<Error>,
}

/// Членство узлов в памяти; клоны разделяют состав. Время — [`Clock`] (у PostgreSQL — часы СУБД).
#[derive(Clone)]
pub struct MemMembership {
    state: Arc<Mutex<State>>,
    clock: Arc<dyn Clock>,
    bus: Option<Arc<dyn EventBus>>,
}

impl std::fmt::Debug for MemMembership {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemMembership")
            .field("state", &self.state)
            .field("clock", &self.clock)
            .field("bus", &self.bus.is_some())
            .finish()
    }
}

impl MemMembership {
    /// Пустой состав с часами `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            clock,
            bus: None,
        }
    }

    /// Публиковать `MembersChanged` в `bus` при входе, выходе и изменении записи узла.
    pub fn with_bus(mut self, bus: Arc<dyn EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Момент истечения записи узла, если запись есть (в том числе просроченная).
    pub fn expires_at(&self, node: &NodeId) -> Option<OffsetDateTime> {
        lock(&self.state).nodes.get(node).map(|(_, at)| *at)
    }

    /// Сигнал после «фиксации»; ошибка шины не отменяет изменения (сигнал может потеряться).
    async fn members_changed(&self) {
        if let Some(bus) = &self.bus {
            let _ = bus
                .publish(Event {
                    tenant: TenantId::DEFAULT,
                    account: None,
                    kind: EventKind::MembersChanged,
                })
                .await;
        }
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
    async fn heartbeat(&self, node: &NodeInfo, ttl: Duration) -> Result<()> {
        let now = self.clock.now();
        let expires_at = add(now, ttl);
        let changed = {
            let mut st = lock(&self.state);
            fault(&mut st)?;
            let prev = st.nodes.insert(node.id.clone(), (node.clone(), expires_at));
            !matches!(prev, Some((info, at)) if at > now && info == *node)
        };
        if changed {
            self.members_changed().await;
        }
        Ok(())
    }

    async fn leave(&self, node: &NodeId) -> Result<()> {
        let now = self.clock.now();
        let was_live = {
            let mut st = lock(&self.state);
            fault(&mut st)?;
            st.nodes.remove(node).is_some_and(|(_, at)| at > now)
        };
        if was_live {
            self.members_changed().await;
        }
        Ok(())
    }

    async fn live_info(&self) -> Result<Vec<NodeInfo>> {
        let now = self.clock.now();
        let mut st = lock(&self.state);
        fault(&mut st)?;
        // Просроченные записи удаляются, как при очистке таблицы в PostgreSQL; на ответ это не влияет.
        st.nodes.retain(|_, (_, expires_at)| *expires_at > now);
        Ok(st.nodes.values().map(|(info, _)| info.clone()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManualClock;
    use crate::MemEventBus;
    use crate::suite::{membership, membership_info};
    use futures::StreamExt;
    use tp_contracts::role::Role;

    const TTL: Duration = Duration::from_secs(30);

    fn node(name: &str) -> NodeId {
        NodeId::new(name).expect("node")
    }

    #[tokio::test]
    async fn passes_contract_suite() {
        let clock = ManualClock::default();
        membership(&MemMembership::new(clock.shared()), TTL, &clock).await;
        membership_info(&MemMembership::new(clock.shared()), TTL, &clock).await;
    }

    #[tokio::test]
    async fn members_changed_signals() {
        let clock = ManualClock::default();
        let bus = MemEventBus::default();
        let m = MemMembership::new(clock.shared()).with_bus(Arc::new(bus.clone()));
        let mut sys = bus.subscribe_system().await.expect("subscribe");
        let mut a = NodeInfo::new(node("a"));
        let changed = || Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind: EventKind::MembersChanged,
        };
        let marker = Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind: EventKind::QueueReady,
        };
        // Вход — сигнал; продление без изменений — нет (проверка: следом приходит метка).
        m.heartbeat(&a, TTL).await.expect("hb");
        assert_eq!(sys.next().await, Some(changed()));
        m.heartbeat(&a, TTL).await.expect("hb");
        bus.publish(marker.clone()).await.expect("marker");
        assert_eq!(sys.next().await, Some(marker.clone()));
        // Изменение ролей — сигнал; возвращение после истечения — сигнал.
        a.roles.insert(Role::Search);
        m.heartbeat(&a, TTL).await.expect("hb");
        assert_eq!(sys.next().await, Some(changed()));
        clock.advance(TTL);
        m.heartbeat(&a, TTL).await.expect("hb");
        assert_eq!(sys.next().await, Some(changed()));
        // Выход живого узла — сигнал; повторный выход — нет.
        m.leave(&a.id).await.expect("leave");
        assert_eq!(sys.next().await, Some(changed()));
        m.leave(&a.id).await.expect("leave again");
        bus.publish(marker.clone()).await.expect("marker");
        assert_eq!(sys.next().await, Some(marker));
    }

    #[tokio::test]
    async fn heartbeat_renew_expire_leave() {
        let clock = ManualClock::default();
        let m = MemMembership::new(clock.shared());
        let (a, b) = (node("mailbox-a"), node("mailbox-b"));
        m.heartbeat(&NodeInfo::new(b.clone()), TTL)
            .await
            .expect("hb");
        m.heartbeat(&NodeInfo::new(a.clone()), TTL)
            .await
            .expect("hb");
        assert_eq!(m.expires_at(&a), Some(clock.now() + TTL));
        assert_eq!(m.live().await.expect("live"), vec![a.clone(), b.clone()]);

        // Продление считает срок от текущего момента; граница срока — уже не жив.
        clock.advance(TTL / 3);
        m.heartbeat(&NodeInfo::new(a.clone()), TTL)
            .await
            .expect("hb");
        clock.advance(TTL - TTL / 3 - Duration::from_secs(1));
        assert_eq!(m.live().await.expect("live"), vec![a.clone(), b.clone()]);
        clock.advance(Duration::from_secs(1));
        assert_eq!(m.live().await.expect("live"), vec![a.clone()]);
        assert_eq!(m.expires_at(&b), None);

        // Более короткий срок сокращает запись.
        m.heartbeat(&NodeInfo::new(a.clone()), Duration::ZERO)
            .await
            .expect("hb");
        assert!(m.live().await.expect("live").is_empty());

        // Просроченный узел возвращается продлением; выход — сразу, повторный выход — не ошибка.
        m.heartbeat(&NodeInfo::new(b.clone()), TTL)
            .await
            .expect("hb");
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
        m.heartbeat(&NodeInfo::new(a.clone()), TTL)
            .await
            .expect("hb");
        assert_eq!(other.live().await.expect("live"), vec![a.clone()]);

        m.fail_next(Error::UNAVAILABLE);
        assert!(
            m.heartbeat(&NodeInfo::new(a.clone()), TTL)
                .await
                .is_err_and(|e| e.is_retryable())
        );
        m.fail_next(Error::UNAVAILABLE);
        assert_eq!(m.live().await, Err(Error::UNAVAILABLE));
        m.fail_next(Error::UNAVAILABLE);
        m.clear_faults();
        assert_eq!(m.live().await.expect("live"), vec![a]);
    }
}
