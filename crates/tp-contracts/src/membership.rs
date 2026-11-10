//! Членство узлов кластера (CONTRACTS.md#Membership; решения 2026-10-30, #45, и 2026-11-01).
//!
//! Узел периодически продлевает свою запись ([`Membership::heartbeat`]) с ограниченным сроком, объявляя роли,
//! адрес служебного канала и площадку ([`NodeInfo`]); живые узлы — записи с непросроченным сроком по часам
//! хранилища. Состав используется для назначения разделов журнала событий ([`crate::events::PartitionAssigner`])
//! и маршрута запросов узел → узел ([`crate::rpc::Rpc`]). Вход и выход узла, изменение его записи реализация
//! публикует сигналом [`EventKind::MembersChanged`](crate::bus::EventKind::MembersChanged) после фиксации.
//! Реализация — PostgreSQL (`tp-meta`), in-memory — `tp-testkit`.

use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::role::Role;

/// Идентификатор узла: постоянное имя экземпляра `thepost` (из конфигурации узла), не пустое.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeId(String);

impl NodeId {
    /// Имя узла: не пустое, без пробельных и управляющих символов, ≤ 255 байт; иначе — `Invalid`.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if !valid_name(&name) {
            return Err(Error::Invalid(format!("invalid node id {name:?}")));
        }
        Ok(NodeId(name))
    }

    /// Имя узла.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for NodeId {
    type Error = Error;

    fn try_from(s: String) -> Result<Self> {
        NodeId::new(s)
    }
}

impl From<NodeId> for String {
    fn from(n: NodeId) -> Self {
        n.0
    }
}

/// Имя площадки (ЦОД) узла: из конфигурации узла, по умолчанию [`SiteId::DEFAULT`]; правила — как у [`NodeId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SiteId(String);

impl SiteId {
    /// Площадка по умолчанию (одна площадка).
    pub const DEFAULT: &'static str = "default";

    /// Имя площадки: не пустое, без пробельных и управляющих символов, ≤ 255 байт; иначе — `Invalid`.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if !valid_name(&name) {
            return Err(Error::Invalid(format!("invalid site {name:?}")));
        }
        Ok(SiteId(name))
    }

    /// Имя площадки.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SiteId {
    fn default() -> Self {
        SiteId(Self::DEFAULT.to_owned())
    }
}

impl fmt::Display for SiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for SiteId {
    type Error = Error;

    fn try_from(s: String) -> Result<Self> {
        SiteId::new(s)
    }
}

impl From<SiteId> for String {
    fn from(s: SiteId) -> Self {
        s.0
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Запись узла в составе кластера.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Узел.
    pub id: NodeId,
    /// Запущенные роли узла (после раскрытия пресетов).
    pub roles: BTreeSet<Role>,
    /// Адрес служебного канала узла для запросов узел → узел (`https://host:port`, `InternalChannel::Node`);
    /// пусто — узел запросов не принимает.
    pub addr: String,
    /// Площадка узла.
    pub site: SiteId,
}

impl NodeInfo {
    /// Узел без ролей и адреса на площадке по умолчанию.
    pub fn new(id: NodeId) -> Self {
        Self {
            id,
            roles: BTreeSet::new(),
            addr: String::new(),
            site: SiteId::default(),
        }
    }

    /// Узел запустил роль `role`.
    pub fn has_role(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }
}

/// Членство узлов: запись узла с продлением. Время — часы хранилища (у PostgreSQL — часы СУБД),
/// чтобы расхождение часов узлов не влияло на состав.
#[async_trait]
pub trait Membership: Send + Sync + 'static {
    /// Создаёт или продлевает запись узла `node.id`: узел жив `ttl` от текущего момента; роли, адрес и площадка
    /// записи заменяются значениями `node`. Период продления — заметно меньше `ttl` (рекомендуется `ttl / 3`).
    /// Новая запись (в том числе после истечения) или изменение ролей, адреса, площадки — сигнал `MembersChanged`.
    async fn heartbeat(&self, node: &NodeInfo, ttl: Duration) -> Result<()>;

    /// Удаляет запись узла (штатная остановка): разделы переходят к другим узлам, не дожидаясь срока.
    /// Отсутствующая запись — не ошибка; удаление живой записи — сигнал `MembersChanged`.
    async fn leave(&self, node: &NodeId) -> Result<()>;

    /// Записи живых узлов (срок не истёк), по возрастанию `id` без повторов.
    async fn live_info(&self) -> Result<Vec<NodeInfo>>;

    /// Живые узлы, по возрастанию без повторов (идентификаторы из [`Membership::live_info`]).
    async fn live(&self) -> Result<Vec<NodeId>> {
        Ok(self.live_info().await?.into_iter().map(|n| n.id).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_safe() {
        fn _membership(_: std::sync::Arc<dyn Membership>) {}
    }

    #[test]
    fn node_id() {
        let n = NodeId::new("mailbox-1").expect("valid");
        assert_eq!(n.as_str(), "mailbox-1");
        assert_eq!(n.to_string(), "mailbox-1");
        for bad in ["", "a b", "a\tb", "a\u{0}"] {
            assert!(NodeId::new(bad).is_err(), "{bad:?}");
        }
        assert!(NodeId::new("x".repeat(256)).is_err());
        assert_eq!(
            serde_json::to_string(&n).expect("ser"),
            r#""mailbox-1""#.to_owned()
        );
        assert_eq!(
            serde_json::from_str::<NodeId>(r#""mailbox-1""#).expect("de"),
            n
        );
        assert!(serde_json::from_str::<NodeId>(r#""""#).is_err());
    }

    #[test]
    fn site_and_info() {
        assert_eq!(SiteId::default().as_str(), "default");
        assert_eq!(SiteId::new("msk-1").expect("site").to_string(), "msk-1");
        for bad in ["", "a b", "a\nb"] {
            assert!(SiteId::new(bad).is_err(), "{bad:?}");
        }
        let mut info = NodeInfo::new(NodeId::new("mailbox-1").expect("node"));
        assert_eq!(info.site, SiteId::default());
        assert!(!info.has_role(Role::Search));
        info.roles.insert(Role::Search);
        info.addr = "https://mailbox-1:7443".to_owned();
        assert!(info.has_role(Role::Search));
        let json = serde_json::to_string(&info).expect("ser");
        assert_eq!(
            json,
            r#"{"id":"mailbox-1","roles":["search"],"addr":"https://mailbox-1:7443","site":"default"}"#
        );
        assert_eq!(serde_json::from_str::<NodeInfo>(&json).expect("de"), info);
        let empty_site = r#"{"id":"n","roles":[],"addr":"","site":""}"#;
        assert!(serde_json::from_str::<NodeInfo>(empty_site).is_err());
    }
}
