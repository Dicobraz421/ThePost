//! Сокеты слушателей (SRS 6.1): TCP IPv4/IPv6, SO_REUSEPORT, backlog, keepalive → tokio `TcpListener`.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use schemars::JsonSchema;
use socket2::{Domain, Protocol, Socket, TcpKeepalive, Type};
use tokio::net::TcpListener;

/// Параметры слушающего сокета. Keepalive наследуется принятыми соединениями (Linux).
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
pub struct ListenerConfig {
    /// Длина очереди принятых, но не обработанных соединений.
    pub backlog: u32,
    /// SO_REUSEPORT: несколько слушателей на одном порту (ядро распределяет соединения).
    pub reuse_port: bool,
    /// IPV6_V6ONLY для адреса IPv6; `false` — `[::]` принимает и IPv4.
    pub only_v6: bool,
    /// Простой до первой пробы keepalive, с; 0 — keepalive выключен.
    pub keepalive_time_secs: u64,
    /// Интервал между пробами keepalive, с.
    pub keepalive_interval_secs: u64,
    /// Число неотвеченных проб до разрыва соединения.
    pub keepalive_retries: u32,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            backlog: 1024,
            reuse_port: true,
            only_v6: false,
            keepalive_time_secs: 300,
            keepalive_interval_secs: 60,
            keepalive_retries: 5,
        }
    }
}

/// Создаёт слушающий сокет на `addr` и передаёт его в tokio. Требует контекста tokio-рантайма.
pub fn bind_tcp(addr: SocketAddr, cfg: &ListenerConfig) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(cfg.only_v6)?;
    }
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(cfg.reuse_port)?;
    if cfg.keepalive_time_secs > 0 {
        let ka = TcpKeepalive::new()
            .with_time(Duration::from_secs(cfg.keepalive_time_secs))
            .with_interval(Duration::from_secs(cfg.keepalive_interval_secs.max(1)))
            .with_retries(cfg.keepalive_retries.max(1));
        socket.set_tcp_keepalive(&ka)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(i32::try_from(cfg.backlog).unwrap_or(i32::MAX))?;
    TcpListener::from_std(socket.into())
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use socket2::SockRef;
    use tokio::net::TcpStream;

    use super::*;

    fn localhost_v4() -> SocketAddr {
        (Ipv4Addr::LOCALHOST, 0).into()
    }

    #[tokio::test]
    async fn two_listeners_share_port_with_reuseport() {
        let cfg = ListenerConfig::default();
        let a = bind_tcp(localhost_v4(), &cfg).expect("first");
        let addr = a.local_addr().expect("addr");
        let b = bind_tcp(addr, &cfg).expect("second on same port");
        assert_eq!(b.local_addr().expect("addr").port(), addr.port());

        // Соединения принимаются: ядро отдаёт их одному из слушателей.
        let _client = TcpStream::connect(addr).await.expect("connect");
        let (stream, _) = tokio::select! {
            r = a.accept() => r,
            r = b.accept() => r,
        }
        .expect("accept");
        let sock = SockRef::from(&stream);
        assert!(
            sock.keepalive().expect("keepalive"),
            "keepalive наследуется"
        );
    }

    #[tokio::test]
    async fn port_is_exclusive_without_reuseport() {
        let cfg = ListenerConfig {
            reuse_port: false,
            keepalive_time_secs: 0,
            ..ListenerConfig::default()
        };
        let a = bind_tcp(localhost_v4(), &cfg).expect("first");
        let err = bind_tcp(a.local_addr().expect("addr"), &cfg).expect_err("second must fail");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn ipv6_listener() {
        let cfg = ListenerConfig {
            only_v6: true,
            ..ListenerConfig::default()
        };
        // В окружениях без IPv6 (часть контейнеров CI) тест не применим.
        let Ok(l) = bind_tcp((Ipv6Addr::LOCALHOST, 0).into(), &cfg) else {
            return;
        };
        let addr = l.local_addr().expect("addr");
        assert!(addr.is_ipv6());
        let _client = TcpStream::connect(addr).await.expect("connect");
        l.accept().await.expect("accept");
    }
}
