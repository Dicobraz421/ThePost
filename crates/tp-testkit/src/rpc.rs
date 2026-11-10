//! In-memory запросы узел → узел (CONTRACTS.md#Rpc; SRS 2.8, решение 2026-11-01).
//!
//! [`MemRpcNet`] — «сеть» узлов в одном процессе: у каждого узла роли, обработчики служб и предел параллельности.
//! [`MemRpc`] — узел этой сети ([`MemRpcNet::node`]). Маршрут — rendezvous-хэширование ключа по узлам с ролью
//! службы, недоступный узел ([`MemRpcNet::set_down`]) — следующий; отказ по матрице разрешений —
//! [`MemRpcNet::forbid`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tp_contracts::membership::NodeId;
use tp_contracts::role::Role;
use tp_contracts::rpc::{Rpc, RpcError, RpcHandler, RpcRequest, RpcService};
use tp_contracts::{Error, Result};

use crate::infra::lock;

struct Node {
    roles: BTreeSet<Role>,
    handlers: HashMap<RpcService, Arc<dyn RpcHandler>>,
    max_in_flight: usize,
    in_flight: usize,
    down: bool,
}

#[derive(Default)]
struct NetState {
    nodes: BTreeMap<NodeId, Node>,
    /// Пары «вызывающий узел, служба», запрещённые матрицей разрешений.
    forbidden: BTreeSet<(NodeId, RpcService)>,
}

/// Сеть узлов в памяти; клоны разделяют состояние.
#[derive(Clone, Default)]
pub struct MemRpcNet {
    state: Arc<Mutex<NetState>>,
}

impl fmt::Debug for MemRpcNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let st = lock(&self.state);
        f.debug_struct("MemRpcNet")
            .field("nodes", &st.nodes.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl MemRpcNet {
    /// Пустая сеть.
    pub fn new() -> Self {
        Self::default()
    }

    /// Добавляет (или заменяет) узел `id` с ролями `roles` и пределом параллельности `max_in_flight` (≥ 1).
    pub fn node(
        &self,
        id: NodeId,
        roles: impl IntoIterator<Item = Role>,
        max_in_flight: usize,
    ) -> MemRpc {
        lock(&self.state).nodes.insert(
            id.clone(),
            Node {
                roles: roles.into_iter().collect(),
                handlers: HashMap::new(),
                max_in_flight: max_in_flight.max(1),
                in_flight: 0,
                down: false,
            },
        );
        MemRpc {
            net: self.clone(),
            id,
        }
    }

    /// Узел недоступен (`true`) или снова доступен; запросы к недоступному уходят следующему узлу.
    pub fn set_down(&self, node: &NodeId, down: bool) {
        if let Some(n) = lock(&self.state).nodes.get_mut(node) {
            n.down = down;
        }
    }

    /// Запрещает узлу `caller` вызывать `service` (матрица разрешений): ответ — `Failed(Forbidden)`.
    pub fn forbid(&self, caller: &NodeId, service: &RpcService) {
        lock(&self.state)
            .forbidden
            .insert((caller.clone(), service.clone()));
    }

    /// Узел-владелец `key` для `service` без учёта доступности (первый в порядке rendezvous).
    pub fn owner(&self, service: &RpcService, key: &str) -> Option<NodeId> {
        route(&lock(&self.state), service, key).into_iter().next()
    }

    /// Число запросов, обрабатываемых узлом сейчас.
    pub fn in_flight(&self, node: &NodeId) -> usize {
        lock(&self.state).nodes.get(node).map_or(0, |n| n.in_flight)
    }
}

/// Узлы с ролью службы в порядке rendezvous-хэширования ключа (FNV-1a по `<ключ>/<узел>`).
fn route(st: &NetState, service: &RpcService, key: &str) -> Vec<NodeId> {
    let mut nodes: Vec<(u64, &NodeId)> = st
        .nodes
        .iter()
        .filter(|(_, n)| n.roles.contains(&service.role()))
        .map(|(id, _)| (score(key, id), id))
        .collect();
    nodes.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    nodes.into_iter().map(|(_, id)| id.clone()).collect()
}

fn score(key: &str, node: &NodeId) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes().chain(*b"/").chain(node.as_str().bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Занятое место обработки; освобождается при завершении или отмене вызова (истечение срока).
struct Slot {
    net: MemRpcNet,
    node: NodeId,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(n) = lock(&self.net.state).nodes.get_mut(&self.node) {
            n.in_flight = n.in_flight.saturating_sub(1);
        }
    }
}

/// Узел сети [`MemRpcNet`].
#[derive(Debug, Clone)]
pub struct MemRpc {
    net: MemRpcNet,
    id: NodeId,
}

impl MemRpc {
    /// Идентификатор узла.
    pub fn id(&self) -> &NodeId {
        &self.id
    }

    /// Выбирает узел и занимает место обработки.
    fn admit(
        &self,
        service: &RpcService,
        key: &str,
    ) -> std::result::Result<(Arc<dyn RpcHandler>, Slot), RpcError> {
        let mut st = lock(&self.net.state);
        let target = route(&st, service, key)
            .into_iter()
            .find(|id| st.nodes.get(id).is_some_and(|n| !n.down))
            .ok_or(RpcError::Unavailable)?;
        if st.forbidden.contains(&(self.id.clone(), service.clone())) {
            return Err(RpcError::Failed(Error::Forbidden));
        }
        let node = st.nodes.get_mut(&target).ok_or(RpcError::Unavailable)?;
        let handler = node
            .handlers
            .get(service)
            .cloned()
            .ok_or(RpcError::Failed(Error::NotFound))?;
        if node.in_flight >= node.max_in_flight {
            return Err(RpcError::Busy);
        }
        node.in_flight += 1;
        Ok((
            handler,
            Slot {
                net: self.net.clone(),
                node: target,
            },
        ))
    }
}

#[async_trait]
impl Rpc for MemRpc {
    async fn call(
        &self,
        service: &RpcService,
        key: &str,
        body: Bytes,
        deadline: Duration,
    ) -> std::result::Result<Bytes, RpcError> {
        let (handler, slot) = self.admit(service, key)?;
        let request = RpcRequest {
            caller: self.id.clone(),
            key: key.to_owned(),
            body,
            deadline,
        };
        let out = tokio::time::timeout(deadline, handler.handle(request)).await;
        drop(slot);
        match out {
            Ok(Ok(body)) => Ok(body),
            Ok(Err(e)) => Err(RpcError::Failed(e)),
            Err(_) => Err(RpcError::Timeout),
        }
    }

    fn serve(&self, service: RpcService, handler: Arc<dyn RpcHandler>) -> Result<()> {
        let mut st = lock(&self.net.state);
        let node = st.nodes.get_mut(&self.id).ok_or(Error::NotFound)?;
        if node.handlers.contains_key(&service) {
            return Err(Error::Conflict);
        }
        node.handlers.insert(service, handler);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::{RpcFixture, rpc};

    struct Name(NodeId);

    #[async_trait]
    impl RpcHandler for Name {
        async fn handle(&self, _: RpcRequest) -> Result<Bytes> {
            Ok(Bytes::from(self.0.to_string()))
        }
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name).expect("node")
    }

    #[tokio::test]
    async fn passes_contract_suite() {
        let net = MemRpcNet::new();
        let nodes = ["a", "b", "c"]
            .into_iter()
            .map(|n| {
                let rpc = net.node(node(n), [Role::Search, Role::Indexer], 2);
                (node(n), Arc::new(rpc) as Arc<dyn Rpc>)
            })
            .collect();
        rpc(&RpcFixture {
            nodes,
            role: Role::Search,
            absent_role: Role::Admin,
            max_in_flight: 2,
        })
        .await;
    }

    #[tokio::test]
    async fn failover_forbid_and_missing_handler() {
        let net = MemRpcNet::new();
        let svc = RpcService::new(Role::Search, "name").expect("svc");
        let rpcs: Vec<MemRpc> = ["a", "b"]
            .into_iter()
            .map(|n| net.node(node(n), [Role::Search], 1))
            .collect();
        // Узел без роли службы в маршрут не входит.
        let caller = net.node(node("edge"), [Role::SmtpIn], 1);
        let key = "tenant/account";
        let owner = net.owner(&svc, key).expect("owner");
        let call = || caller.call(&svc, key, Bytes::new(), Duration::from_secs(5));

        // Владелец без зарегистрированной службы — NotFound.
        assert_eq!(call().await, Err(RpcError::Failed(Error::NotFound)));
        for r in &rpcs {
            r.serve(svc.clone(), Arc::new(Name(r.id().clone())))
                .expect("serve");
        }
        assert_eq!(call().await, Ok(Bytes::from(owner.to_string())));

        // Отказ владельца — следующий узел; оба недоступны — Unavailable.
        net.set_down(&owner, true);
        let next = call().await.expect("failover");
        assert_ne!(next, Bytes::from(owner.to_string()));
        for r in &rpcs {
            net.set_down(r.id(), true);
        }
        assert_eq!(call().await, Err(RpcError::Unavailable));
        for r in &rpcs {
            net.set_down(r.id(), false);
        }
        assert_eq!(call().await, Ok(Bytes::from(owner.to_string())));

        // Матрица разрешений: запрет для одного вызывающего.
        net.forbid(caller.id(), &svc);
        assert_eq!(call().await, Err(RpcError::Failed(Error::Forbidden)));
        assert!(
            rpcs[0]
                .call(&svc, key, Bytes::new(), Duration::from_secs(5))
                .await
                .is_ok()
        );
        assert_eq!(net.in_flight(&owner), 0);
    }
}
