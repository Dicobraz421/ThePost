//! Запросы узел → узел (CONTRACTS.md#Rpc; SRS 2.8, решение 2026-11-01).
//!
//! Синхронный вызов службы роли на другом узле: ответ, «занят» или истечение срока. Маршрут — узлы с нужной ролью
//! из состава кластера ([`crate::membership::Membership::live_info`]), владелец ключа — rendezvous-хэширование,
//! при отказе узла — следующий. Параллельность обработки на узле ограничена: при превышении — [`RpcError::Busy`],
//! который вызывающий переводит во временный отказ (не повторяет на другом узле). Матрица разрешений
//! «роль → службы» проверяется на принимающем узле по сертификату вызывающего. Транспорт — HTTPS/2 с mTLS
//! (`InternalChannel::Node`), реализация — `tp-cluster`; в одном процессе — вызов функции; in-memory — `tp-testkit`.
//!
//! ```
//! use tp_contracts::Error;
//! use tp_contracts::role::Role;
//! use tp_contracts::rpc::{RpcError, RpcService};
//!
//! let search = RpcService::new(Role::Search, "query")?;
//! assert_eq!(search.to_string(), "search/query");
//! assert!(RpcService::new(Role::Search, "Query!").is_err());
//! assert_eq!(Error::from(RpcError::Busy), Error::UNAVAILABLE);
//! assert_eq!(Error::from(RpcError::Failed(Error::Forbidden)), Error::Forbidden);
//! # Ok::<(), tp_contracts::Error>(())
//! ```

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::{Error, Result};
use crate::membership::NodeId;
use crate::role::Role;

/// Служба узла: роль, которая её обслуживает, и имя службы внутри роли (`search/query`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RpcService {
    role: Role,
    name: String,
}

impl RpcService {
    /// Служба `name` роли `role`; имя — `[a-z0-9._-]`, 1…64 символа, иначе — `Invalid`.
    pub fn new(role: Role, name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
        if !ok {
            return Err(Error::Invalid(format!("invalid rpc service name {name:?}")));
        }
        Ok(Self { role, name })
    }

    /// Роль, которая обслуживает службу.
    pub fn role(&self) -> Role {
        self.role
    }

    /// Имя службы внутри роли.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for RpcService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.role, self.name)
    }
}

/// Запрос к обработчику службы на принимающем узле.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcRequest {
    /// Вызывающий узел (по сертификату mTLS; матрица разрешений уже проверена).
    pub caller: NodeId,
    /// Ключ маршрута (например, `<tenant>/<учётная запись>`).
    pub key: String,
    /// Тело запроса (формат — дело службы).
    pub body: Bytes,
    /// Остаток срока вызова на момент передачи обработчику.
    pub deadline: Duration,
}

/// Ошибка вызова службы.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RpcError {
    /// Узел-владелец занят (параллельность обработки превышена): временный отказ, без повтора на другом узле.
    #[error("rpc: busy")]
    Busy,
    /// Срок вызова истёк до ответа.
    #[error("rpc: timeout")]
    Timeout,
    /// Нет живого узла с ролью службы или все узлы маршрута недоступны.
    #[error("rpc: unavailable")]
    Unavailable,
    /// Ответ обработчика с ошибкой или отказ принимающего узла (`Forbidden` — матрица разрешений,
    /// `NotFound` — служба не зарегистрирована на узле).
    #[error("rpc: {0}")]
    Failed(Error),
}

impl RpcError {
    /// Временный отказ: `Busy`, `Timeout`, `Unavailable` или повторяемая ошибка обработчика.
    pub fn is_retryable(&self) -> bool {
        match self {
            RpcError::Busy | RpcError::Timeout | RpcError::Unavailable => true,
            RpcError::Failed(e) => e.is_retryable(),
        }
    }
}

impl From<RpcError> for Error {
    /// `Busy`, `Timeout`, `Unavailable` — `Unavailable` (временный отказ); `Failed(e)` — `e`.
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::Busy | RpcError::Timeout | RpcError::Unavailable => Error::UNAVAILABLE,
            RpcError::Failed(e) => e,
        }
    }
}

/// Обработчик службы на принимающем узле; регистрирует роль при запуске ([`Rpc::serve`]).
#[async_trait]
pub trait RpcHandler: Send + Sync + 'static {
    /// Обрабатывает запрос; ошибка возвращается вызывающему как [`RpcError::Failed`].
    async fn handle(&self, request: RpcRequest) -> Result<Bytes>;
}

/// Запросы узел → узел: вызов служб других узлов и регистрация служб этого узла.
#[async_trait]
pub trait Rpc: Send + Sync + 'static {
    /// Вызывает `service` на узле-владельце `key` (rendezvous-хэширование по узлам с ролью службы; недоступный
    /// узел — следующий) со сроком `deadline` от момента вызова.
    async fn call(
        &self,
        service: &RpcService,
        key: &str,
        body: Bytes,
        deadline: Duration,
    ) -> std::result::Result<Bytes, RpcError>;

    /// Регистрирует обработчик службы на этом узле; повторная регистрация службы — `Conflict`.
    fn serve(&self, service: RpcService, handler: Arc<dyn RpcHandler>) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_safe() {
        fn _rpc(_: Arc<dyn Rpc>) {}
        fn _handler(_: Arc<dyn RpcHandler>) {}
    }

    #[test]
    fn service_names() {
        let s = RpcService::new(Role::Admin, "node.status-v1_2").expect("service");
        assert_eq!((s.role(), s.name()), (Role::Admin, "node.status-v1_2"));
        for bad in ["", "a/b", "A", "a b", &"x".repeat(65)] {
            assert!(RpcService::new(Role::Admin, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn errors() {
        for e in [RpcError::Busy, RpcError::Timeout, RpcError::Unavailable] {
            assert!(e.is_retryable());
            assert_eq!(Error::from(e), Error::UNAVAILABLE);
        }
        assert!(!RpcError::Failed(Error::Forbidden).is_retryable());
        assert!(RpcError::Failed(Error::UNAVAILABLE).is_retryable());
        assert_eq!(
            RpcError::Failed(Error::invalid("x")).to_string(),
            "rpc: invalid: x"
        );
    }
}
