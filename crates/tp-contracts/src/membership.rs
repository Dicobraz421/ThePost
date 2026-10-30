//! Членство узлов кластера (CONTRACTS.md#Membership; решение 2026-10-30, #45).
//!
//! Узел периодически продлевает свою запись ([`Membership::heartbeat`]) с ограниченным сроком; живые узлы —
//! записи с непросроченным сроком по часам хранилища. Состав используется для назначения разделов журнала
//! событий ([`crate::events::PartitionAssigner`]). Реализация — PostgreSQL (`tp-meta`), in-memory — `tp-testkit`.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Идентификатор узла: постоянное имя экземпляра `thepost` (из конфигурации узла), не пустое.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeId(String);

impl NodeId {
    /// Имя узла: не пустое, без пробельных и управляющих символов, ≤ 255 байт; иначе — `Invalid`.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.is_empty()
            || name.len() > 255
            || name.chars().any(|c| c.is_whitespace() || c.is_control())
        {
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

/// Членство узлов: запись узла с продлением. Время — часы хранилища (у PostgreSQL — часы СУБД),
/// чтобы расхождение часов узлов не влияло на состав.
#[async_trait]
pub trait Membership: Send + Sync + 'static {
    /// Создаёт или продлевает запись узла: узел жив `ttl` от текущего момента.
    /// Период продления — заметно меньше `ttl` (рекомендуется `ttl / 3`).
    async fn heartbeat(&self, node: &NodeId, ttl: Duration) -> Result<()>;

    /// Удаляет запись узла (штатная остановка): разделы переходят к другим узлам, не дожидаясь срока.
    /// Отсутствующая запись — не ошибка.
    async fn leave(&self, node: &NodeId) -> Result<()>;

    /// Живые узлы (срок записи не истёк), по возрастанию без повторов.
    async fn live(&self) -> Result<Vec<NodeId>>;
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
}
