//! Блокировки источников и учётных записей при переборе (CONTRACTS.md#AbuseBlocks; решение 2026-11-01, SRS 6.3).
//!
//! Счётчики попыток ведёт узел (`tp-auth`); при превышении порога — [`AbuseBlocks::block`]. Факт блокировки хранится
//! в PostgreSQL (`tp-meta`, по организации) и после фиксации рассылается сигналом
//! [`EventKind::Blocked`](crate::bus::EventKind::Blocked); узлы держат локальный набор блокировок, обновляемый
//! по сигналу и при [`EventKind::Resync`](crate::bus::EventKind::Resync) ([`AbuseBlocks::active`]).
//!
//! ```
//! use std::net::IpAddr;
//! use tp_contracts::abuse::BlockSubject;
//!
//! let ip: IpAddr = "192.0.2.17".parse().expect("ip");
//! let net = BlockSubject::net("192.0.2.200".parse().expect("ip"), 24)?;
//! assert_eq!(net.to_string(), "192.0.2.0/24");
//! assert!(net.covers(&BlockSubject::Ip(ip)));
//! assert!(!BlockSubject::Ip(ip).covers(&net));
//! assert!(BlockSubject::net(ip, 33).is_err());
//! # Ok::<(), tp_contracts::Error>(())
//! ```

use std::fmt;
use std::net::IpAddr;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::ids::{AccountId, TenantId};

/// Что блокируется: адрес, подсеть или учётная запись.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockSubject {
    /// Адрес источника.
    Ip(IpAddr),
    /// Подсеть источников; строится через [`BlockSubject::net`] (биты узла обнулены).
    Net {
        /// Адрес подсети.
        addr: IpAddr,
        /// Длина префикса: ≤ 32 для IPv4, ≤ 128 для IPv6.
        prefix: u8,
    },
    /// Учётная запись (вход под ней с любых адресов).
    Account(AccountId),
}

impl BlockSubject {
    /// Подсеть `addr/prefix` с обнулёнными битами узла; префикс больше длины адреса — `Invalid`.
    pub fn net(addr: IpAddr, prefix: u8) -> Result<Self> {
        if prefix > max_prefix(addr) {
            return Err(Error::Invalid(format!(
                "invalid prefix /{prefix} for {addr}"
            )));
        }
        Ok(BlockSubject::Net {
            addr: mask(addr, prefix),
            prefix,
        })
    }

    /// Блокировка `self` распространяется на `other`: тот же субъект, адрес внутри подсети или подсеть
    /// внутри подсети. Учётная запись покрывает только себя.
    pub fn covers(&self, other: &BlockSubject) -> bool {
        match (*self, *other) {
            (BlockSubject::Net { addr, prefix }, BlockSubject::Ip(ip)) => in_net(addr, prefix, ip),
            (
                BlockSubject::Net { addr, prefix },
                BlockSubject::Net {
                    addr: inner,
                    prefix: inner_prefix,
                },
            ) => inner_prefix >= prefix && in_net(addr, prefix, inner),
            (a, b) => a == b,
        }
    }
}

impl fmt::Display for BlockSubject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockSubject::Ip(ip) => write!(f, "{ip}"),
            BlockSubject::Net { addr, prefix } => write!(f, "{addr}/{prefix}"),
            BlockSubject::Account(account) => write!(f, "account {account}"),
        }
    }
}

const fn max_prefix(addr: IpAddr) -> u8 {
    match addr {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    }
}

fn mask(addr: IpAddr, prefix: u8) -> IpAddr {
    match addr {
        IpAddr::V4(a) => {
            let m = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V4((u32::from(a) & m).into())
        }
        IpAddr::V6(a) => {
            let m = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V6((u128::from(a) & m).into())
        }
    }
}

/// `ip` в подсети `addr/prefix` (семейства адресов совпадают).
fn in_net(addr: IpAddr, prefix: u8, ip: IpAddr) -> bool {
    addr.is_ipv4() == ip.is_ipv4()
        && prefix <= max_prefix(addr)
        && mask(addr, prefix) == mask(ip, prefix)
}

/// Действующая блокировка.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbuseBlock {
    /// Субъект.
    pub subject: BlockSubject,
    /// Срок блокировки (UTC).
    pub until: OffsetDateTime,
    /// Причина для администратора и журнала (без паролей и содержимого писем).
    pub reason: String,
}

/// Хранилище блокировок организации. Время — часы хранилища (у PostgreSQL — часы СУБД). Реализация —
/// `tp-meta` (META-27), in-memory — `tp-testkit`. Реализация после фиксации `block` публикует сигнал
/// `Event { tenant, account: None, kind: Blocked { subject } }`.
#[async_trait]
pub trait AbuseBlocks: Send + Sync + 'static {
    /// Блокирует `subject` до `until`. Действующая блокировка того же субъекта продлевается до более
    /// позднего срока (не сокращается), причина — от последнего вызова. Срок в прошлом — `Invalid`.
    async fn block(
        &self,
        tenant: TenantId,
        subject: &BlockSubject,
        until: OffsetDateTime,
        reason: &str,
    ) -> Result<()>;

    /// Срок блокировки `subject` с учётом подсетей, покрывающих адрес ([`BlockSubject::covers`]):
    /// наибольший из действующих; `None` — не заблокирован. Истёкшие блокировки не учитываются.
    async fn blocked(
        &self,
        tenant: TenantId,
        subject: &BlockSubject,
    ) -> Result<Option<OffsetDateTime>>;

    /// Снимает блокировку именно этого субъекта (администратор); отсутствующая — не ошибка.
    async fn unblock(&self, tenant: TenantId, subject: &BlockSubject) -> Result<()>;

    /// Действующие блокировки организации (локальный набор узла после запуска и `Resync`), без порядка.
    async fn active(&self, tenant: TenantId) -> Result<Vec<AbuseBlock>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip")
    }

    #[test]
    fn object_safe() {
        fn _blocks(_: std::sync::Arc<dyn AbuseBlocks>) {}
    }

    #[test]
    fn nets_and_cover() {
        let net = BlockSubject::net(ip("10.1.2.3"), 16).expect("net");
        assert_eq!(
            net,
            BlockSubject::Net {
                addr: ip("10.1.0.0"),
                prefix: 16
            }
        );
        assert!(net.covers(&BlockSubject::Ip(ip("10.1.255.1"))));
        assert!(!net.covers(&BlockSubject::Ip(ip("10.2.0.1"))));
        assert!(net.covers(&BlockSubject::net(ip("10.1.7.0"), 24).expect("net")));
        assert!(!net.covers(&BlockSubject::net(ip("10.0.0.0"), 8).expect("net")));
        assert!(net.covers(&net));
        // Семейства адресов не смешиваются; /0 покрывает всё своё семейство.
        let all4 = BlockSubject::net(ip("1.2.3.4"), 0).expect("net");
        assert!(all4.covers(&BlockSubject::Ip(ip("203.0.113.9"))));
        assert!(!all4.covers(&BlockSubject::Ip(ip("::ffff:203.0.113.9"))));
        let v6 = BlockSubject::net(ip("2001:db8::1"), 32).expect("net");
        assert_eq!(v6.to_string(), "2001:db8::/32");
        assert!(v6.covers(&BlockSubject::Ip(ip("2001:db8:ffff::1"))));
        assert!(BlockSubject::net(ip("2001:db8::1"), 128).is_ok());
        assert!(BlockSubject::net(ip("2001:db8::1"), 129).is_err());
        // Учётная запись покрывает только себя.
        let a = BlockSubject::Account(AccountId::new());
        assert!(a.covers(&a));
        assert!(!a.covers(&BlockSubject::Account(AccountId::new())));
        assert!(!BlockSubject::Ip(ip("10.1.0.1")).covers(&net));
    }

    #[test]
    fn serde_shape() {
        let s = BlockSubject::Ip(ip("192.0.2.1"));
        assert_eq!(
            serde_json::to_string(&s).expect("ser"),
            r#"{"ip":"192.0.2.1"}"#
        );
        let net = BlockSubject::net(ip("192.0.2.0"), 24).expect("net");
        let json = serde_json::to_string(&net).expect("ser");
        assert_eq!(json, r#"{"net":{"addr":"192.0.2.0","prefix":24}}"#);
        assert_eq!(
            serde_json::from_str::<BlockSubject>(&json).expect("de"),
            net
        );
    }
}
