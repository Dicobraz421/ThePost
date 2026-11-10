//! Запросы узел → узел: маршрут, допуск и матрица разрешений (CONTRACTS.md#Rpc; SRS 2.8, решение 2026-11-01).
//!
//! Части [`tp_contracts::rpc::Rpc`], не зависящие от транспорта:
//! - [`route`] — узлы маршрута: живые записи ([`Membership::live_info`](tp_contracts::membership::Membership::live_info))
//!   с ролью службы и адресом служебного канала, по убыванию веса [`Rendezvous`] для ключа (владелец, затем
//!   следующие — при отказе узла);
//! - [`Admission`] — предел параллельности обработки на принимающем узле: при превышении — [`RpcError::Busy`];
//! - [`Permissions`] — матрица «роль вызывающего → службы», проверяется на принимающем узле по ролям узла,
//!   которого назвал сертификат вызывающего;
//! - [`RpcConfig`] — секция конфигурации.
//!
//! Транспорт HTTPS с mTLS (`InternalChannel::Node`) — [`crate::node_rpc`] (CLU-12).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tp_contracts::config::duration;
use tp_contracts::membership::NodeInfo;
use tp_contracts::role::Role;
use tp_contracts::rpc::{RpcError, RpcService};
use tp_contracts::tls::{InternalChannel, InternalTls, Profile};
use tp_contracts::{Error, Result};

use crate::hashing::Rendezvous;

/// Секция запросов узел → узел. Незаданные поля — по умолчанию, неизвестные — ошибка разбора.
///
/// ```yaml
/// max_in_flight: 64
/// connect_timeout: 2s
/// tls:
///   ca_file: /etc/thepost/cluster-ca.pem
///   cert_file: /etc/thepost/node.pem     # CN — идентификатор узла
///   key_file: /etc/thepost/node-key.pem
/// permissions:
///   jmap: [search/query]
///   admin: [admin/*]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RpcConfig {
    /// Запросов, обрабатываемых узлом одновременно; сверх — ответ «занят» (не меньше 1).
    pub max_in_flight: usize,
    /// Срок установления соединения с узлом маршрута; отказ — переход к следующему узлу.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub connect_timeout: Duration,
    /// Матрица разрешений: роль вызывающего узла → службы `<роль>/<имя>` или все службы роли `<роль>/*`.
    /// Не указано — запрещено (минимальные привилегии, SRS 6.8).
    pub permissions: BTreeMap<Role, BTreeSet<String>>,
    /// Срок рукопожатия mTLS входящего соединения.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub handshake_timeout: Duration,
    /// Пауза после ошибки приёма соединения (нехватка дескрипторов и т. п.).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub accept_retry_delay: Duration,
    /// Сколько использовать прочитанный состав кластера (маршрут и роли вызывающего); `0s` — читать каждый раз.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub members_ttl: Duration,
    /// Наибольший размер запроса (ключ и тело), байт.
    pub max_body_bytes: usize,
    /// TLS внутреннего канала: УЦ узлов (`ca_file`), сертификат узла (`cert_file`, CN — идентификатор узла) и
    /// ключ (`key_file`) — обязательны при запуске транспорта; открытый канал и `server_name` не допускаются.
    pub tls: InternalTls,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 64,
            connect_timeout: Duration::from_secs(2),
            permissions: BTreeMap::new(),
            handshake_timeout: Duration::from_secs(5),
            accept_retry_delay: Duration::from_millis(100),
            members_ttl: Duration::from_secs(1),
            max_body_bytes: 16 << 20,
            tls: InternalTls::default(),
        }
    }
}

impl RpcConfig {
    /// Проверка: ненулевые пределы, разбираемая матрица разрешений, только TLS (открытый канал запрещён в любом
    /// профиле: без сертификата нет идентичности вызывающего узла).
    pub fn check(&self) -> Result<()> {
        if self.max_in_flight == 0 {
            return Err(Error::invalid("rpc.max_in_flight must be at least 1"));
        }
        if self.connect_timeout.is_zero() {
            return Err(Error::invalid("rpc.connect_timeout must be positive"));
        }
        if self.handshake_timeout.is_zero() {
            return Err(Error::invalid("rpc.handshake_timeout must be positive"));
        }
        if self.max_body_bytes == 0 {
            return Err(Error::invalid("rpc.max_body_bytes must be at least 1"));
        }
        self.tls.validate(InternalChannel::Node, Profile::Prod)?;
        if self.tls.server_name.is_some() {
            return Err(Error::invalid(
                "rpc.tls.server_name is not supported: the server name is the host of each node address",
            ));
        }
        Permissions::from_config(self).map(drop)
    }
}

/// Разрешённые службы роли.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Grant {
    /// Роли, все службы которых разрешены (`<роль>/*`).
    any_of: BTreeSet<Role>,
    /// Отдельные службы.
    services: BTreeSet<RpcService>,
}

/// Матрица разрешений «роль вызывающего → службы».
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Permissions {
    grants: BTreeMap<Role, Grant>,
}

impl Permissions {
    /// Матрица из секции; запись не вида `<роль>/<имя>` или `<роль>/*` — `Invalid`.
    pub fn from_config(config: &RpcConfig) -> Result<Self> {
        let mut grants = BTreeMap::new();
        for (caller, entries) in &config.permissions {
            let mut grant = Grant::default();
            for entry in entries {
                let invalid = || {
                    Error::Invalid(format!(
                        "rpc.permissions.{caller}: invalid service {entry:?}"
                    ))
                };
                let (role, name) = entry.split_once('/').ok_or_else(invalid)?;
                let role: Role = role.parse().map_err(|_| invalid())?;
                if name == "*" {
                    grant.any_of.insert(role);
                } else {
                    grant
                        .services
                        .insert(RpcService::new(role, name).map_err(|_| invalid())?);
                }
            }
            grants.insert(*caller, grant);
        }
        Ok(Self { grants })
    }

    /// Узел с ролями `caller_roles` может вызывать `service`: разрешено хотя бы одной его роли.
    pub fn allows(&self, caller_roles: &BTreeSet<Role>, service: &RpcService) -> bool {
        caller_roles.iter().any(|role| {
            self.grants
                .get(role)
                .is_some_and(|g| g.any_of.contains(&service.role()) || g.services.contains(service))
        })
    }

    /// Проверка вызова на принимающем узле: `Failed(Forbidden)`, если не разрешено.
    pub fn check(
        &self,
        caller_roles: &BTreeSet<Role>,
        service: &RpcService,
    ) -> std::result::Result<(), RpcError> {
        if self.allows(caller_roles, service) {
            Ok(())
        } else {
            Err(RpcError::Failed(Error::Forbidden))
        }
    }
}

/// Узлы маршрута `service` для ключа `key`: живые узлы с ролью службы и адресом служебного канала, по убыванию
/// веса rendezvous-хэширования (первый — владелец ключа, далее — следующие при отказе). Пусто — `Unavailable`.
pub fn route<'a>(
    live: &'a [NodeInfo],
    service: &RpcService,
    key: &str,
) -> std::result::Result<Vec<&'a NodeInfo>, RpcError> {
    let candidates: Vec<&NodeInfo> = live
        .iter()
        .filter(|n| n.has_role(service.role()) && !n.addr.is_empty())
        .collect();
    let ring = Rendezvous::new(candidates.iter().map(|n| n.id.as_str()));
    let ordered: Vec<&NodeInfo> = ring
        .ranked_of(key.as_bytes())
        .into_iter()
        .filter_map(|id| candidates.iter().copied().find(|n| n.id.as_str() == id))
        .collect();
    if ordered.is_empty() {
        return Err(RpcError::Unavailable);
    }
    Ok(ordered)
}

/// Предел параллельности обработки запросов на узле; клоны разделяют предел.
#[derive(Debug, Clone)]
pub struct Admission {
    slots: Arc<Semaphore>,
}

impl Admission {
    /// Предел `max_in_flight` запросов (не меньше 1).
    pub fn new(max_in_flight: usize) -> Self {
        let max = max_in_flight.clamp(1, Semaphore::MAX_PERMITS);
        Self {
            slots: Arc::new(Semaphore::new(max)),
        }
    }

    /// Занимает место обработки до удаления разрешения; мест нет — [`RpcError::Busy`] без ожидания.
    pub fn try_admit(&self) -> std::result::Result<OwnedSemaphorePermit, RpcError> {
        Arc::clone(&self.slots)
            .try_acquire_owned()
            .map_err(|_| RpcError::Busy)
    }

    /// Свободных мест сейчас.
    pub fn available(&self) -> usize {
        self.slots.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use tp_contracts::membership::NodeId;

    use super::*;

    fn info(name: &str, roles: &[Role], addr: &str) -> NodeInfo {
        let mut n = NodeInfo::new(NodeId::new(name).expect("node"));
        n.roles = roles.iter().copied().collect();
        n.addr = addr.to_owned();
        n
    }

    fn svc(role: Role, name: &str) -> RpcService {
        RpcService::new(role, name).expect("service")
    }

    fn ids(nodes: &[&NodeInfo]) -> Vec<String> {
        nodes.iter().map(|n| n.id.to_string()).collect()
    }

    #[test]
    fn route_filters_by_role_and_address_and_ranks() {
        let live = vec![
            info("n1", &[Role::Search], "https://n1:7443"),
            info("n2", &[Role::Search, Role::Jmap], "https://n2:7443"),
            info("n3", &[Role::Jmap], "https://n3:7443"),
            info("n4", &[Role::Search], ""),
            info("n5", &[Role::Search], "https://n5:7443"),
        ];
        let query = svc(Role::Search, "query");
        let r = route(&live, &query, "t/a").expect("route");
        let mut got = ids(&r);
        // Только узлы с ролью и адресом; порядок — rendezvous по ключу.
        let ring = Rendezvous::new(["n1", "n2", "n5"]);
        assert_eq!(got, ring.ranked_of(b"t/a"));
        got.sort();
        assert_eq!(got, ["n1", "n2", "n5"]);
        // Владелец одинаков при любом порядке записей.
        let mut shuffled = live.clone();
        shuffled.reverse();
        assert_eq!(
            ids(&route(&shuffled, &query, "t/a").expect("route")),
            ids(&r)
        );
        // Отказ владельца: следующий в порядке маршрута становится владельцем.
        let without_owner: Vec<NodeInfo> =
            live.iter().filter(|n| n.id != r[0].id).cloned().collect();
        assert_eq!(
            route(&without_owner, &query, "t/a").expect("route")[0].id,
            r[1].id
        );
        assert_eq!(
            route(&live, &svc(Role::Admin, "status"), "k").expect_err("no admin"),
            RpcError::Unavailable
        );
        assert_eq!(
            route(&[], &query, "k").expect_err("empty"),
            RpcError::Unavailable
        );
    }

    #[test]
    fn admission_limits_in_flight() {
        let a = Admission::new(2);
        let p1 = a.try_admit().expect("1");
        let _p2 = a.clone().try_admit().expect("2");
        assert_eq!(a.try_admit().expect_err("busy"), RpcError::Busy);
        drop(p1);
        assert_eq!(a.available(), 1);
        let _p3 = a.try_admit().expect("freed");
        assert_eq!(Admission::new(0).available(), 1);
    }

    #[test]
    fn permissions_matrix() {
        let config: RpcConfig = serde_yaml_ng::from_str(
            "permissions:\n  jmap: [search/query]\n  admin: [admin/*, search/stats]\n",
        )
        .expect("yaml");
        config.check().expect("check");
        let p = Permissions::from_config(&config).expect("matrix");
        let roles = |r: &[Role]| r.iter().copied().collect::<BTreeSet<Role>>();
        let query = svc(Role::Search, "query");
        assert!(p.allows(&roles(&[Role::Jmap]), &query));
        assert!(p.allows(&roles(&[Role::Imap, Role::Jmap]), &query));
        assert!(!p.allows(&roles(&[Role::Imap]), &query));
        assert!(!p.allows(&roles(&[Role::Admin]), &query));
        assert!(p.allows(&roles(&[Role::Admin]), &svc(Role::Search, "stats")));
        assert!(p.allows(&roles(&[Role::Admin]), &svc(Role::Admin, "node.status")));
        assert!(!p.allows(&roles(&[]), &query));
        assert_eq!(
            p.check(&roles(&[Role::Imap]), &query),
            Err(RpcError::Failed(Error::Forbidden))
        );
        // По умолчанию запрещено всё.
        let none = Permissions::from_config(&RpcConfig::default()).expect("default");
        assert!(!none.allows(&roles(&[Role::Admin]), &query));
    }

    #[test]
    fn parses_yaml_section() {
        let c: RpcConfig =
            serde_yaml_ng::from_str("max_in_flight: 8\nconnect_timeout: 500ms\n").expect("yaml");
        assert_eq!(
            (c.max_in_flight, c.connect_timeout),
            (8, Duration::from_millis(500))
        );
        assert_eq!(
            serde_yaml_ng::from_str::<RpcConfig>("{}").expect("empty"),
            RpcConfig::default()
        );
        for bad in [
            "connect_timeout: 2",
            "max_in_flight: -1",
            "permissions:\n  nobody: [search/query]\n",
            "limit: 1",
        ] {
            assert!(serde_yaml_ng::from_str::<RpcConfig>(bad).is_err(), "{bad}");
        }
        for bad in [
            "max_in_flight: 0\n",
            "connect_timeout: 0s\n",
            "handshake_timeout: 0s\n",
            "max_body_bytes: 0\n",
            "tls:\n  allow_plaintext: true\n",
            "tls:\n  server_name: n1\n",
            "tls:\n  cert_file: /x.pem\n",
            "permissions:\n  jmap: [query]\n",
            "permissions:\n  jmap: [nobody/query]\n",
            "permissions:\n  jmap: [search/Query]\n",
        ] {
            let c: RpcConfig = serde_yaml_ng::from_str(bad).expect("yaml");
            assert!(c.check().is_err(), "{bad}");
        }
    }
}
