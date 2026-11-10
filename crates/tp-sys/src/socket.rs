//! Сокеты слушателей (SRS 6.1): TCP IPv4/IPv6, SO_REUSEPORT, backlog, keepalive → tokio `TcpListener`;
//! слушающие сокеты, переданные systemd (`LISTEN_FDS`, sd_listen_fds(3)), → `std::net::TcpListener`.

use std::collections::BTreeSet;
use std::io;
use std::net::SocketAddr;
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Mutex;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use socket2::{Domain, Protocol, SockRef, Socket, TcpKeepalive, Type};
use tokio::net::TcpListener;

/// Параметры слушающего сокета. Keepalive наследуется принятыми соединениями (Linux).
/// Разбирается из секции конфигурации; пропущенные поля — значения по умолчанию.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
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

/// Первый дескриптор, переданный systemd (`SD_LISTEN_FDS_START`).
const LISTEN_FDS_START: RawFd = 3;

/// Номера дескрипторов, уже захваченных [`adopt_listen_fd`] в этом процессе.
static CLAIMED: Mutex<BTreeSet<RawFd>> = Mutex::new(BTreeSet::new());

/// Берёт во владение слушающий TCP-сокет, переданный systemd, по номеру дескриптора `fd` и возвращает его как
/// `std::net::TcpListener` (режим блокировки не меняется — для tokio вызвать `set_nonblocking(true)`).
///
/// Проверки до захвата: `LISTEN_PID` — текущий процесс и `fd` в диапазоне `3..3 + LISTEN_FDS`; дескриптор открыт и
/// это сокет; `SO_ACCEPTCONN` (слушает), `SOCK_STREAM`, семейство IPv4/IPv6, протокол TCP. Номер захватывается один
/// раз за время жизни процесса: повторный вызов с тем же `fd` — ошибка `AlreadyExists` (даже если первый сокет
/// уже закрыт и номер занят другим объектом). Захваченному дескриптору ставится `FD_CLOEXEC`.
/// Неудачная проверка дескриптор не захватывает и не закрывает.
pub fn adopt_listen_fd(fd: RawFd) -> io::Result<std::net::TcpListener> {
    let mut claimed = CLAIMED.lock().unwrap_or_else(|e| e.into_inner());
    adopt_in(
        fd,
        std::process::id(),
        |n| std::env::var(n).ok(),
        &mut claimed,
    )
}

/// [`adopt_listen_fd`] с явными pid, переменными окружения и набором захваченных номеров (тесты).
fn adopt_in(
    fd: RawFd,
    pid: u32,
    var: impl Fn(&str) -> Option<String>,
    claimed: &mut BTreeSet<RawFd>,
) -> io::Result<std::net::TcpListener> {
    let invalid = |m: String| io::Error::new(io::ErrorKind::InvalidInput, format!("fd {fd}: {m}"));
    if claimed.contains(&fd) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("fd {fd}: дескриптор уже захвачен"),
        ));
    }
    let listen_pid = var("LISTEN_PID").and_then(|p| p.trim().parse::<u32>().ok());
    if listen_pid != Some(pid) {
        return Err(invalid(
            "LISTEN_PID не задан или не текущего процесса".into(),
        ));
    }
    let count = var("LISTEN_FDS")
        .and_then(|n| n.trim().parse::<RawFd>().ok())
        .filter(|n| *n >= 0)
        .ok_or_else(|| invalid("LISTEN_FDS не задан или неверен".into()))?;
    if fd < LISTEN_FDS_START || fd - LISTEN_FDS_START >= count {
        return Err(invalid(format!(
            "вне переданных systemd дескрипторов {LISTEN_FDS_START}..{}",
            LISTEN_FDS_START.saturating_add(count)
        )));
    }
    // Открыт ли дескриптор и сокет ли это — без системных вызовов над самим номером.
    let target = std::fs::read_link(format!("/proc/self/fd/{fd}"))
        .map_err(|e| invalid(format!("дескриптор не открыт: {e}")))?;
    if !target.to_string_lossy().starts_with("socket:") {
        return Err(invalid(format!("не сокет: {}", target.display())));
    }
    // SAFETY: дескриптор открыт (проверено выше) и передан процессу systemd (`LISTEN_PID`, диапазон
    // `LISTEN_FDS`); заимствование живёт только до конца проверок ниже и дескриптор не закрывает.
    #[allow(unsafe_code)]
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let sock = SockRef::from(&borrowed);
    if !sock.is_listener()? {
        return Err(invalid("сокет не слушает (нет SO_ACCEPTCONN)".into()));
    }
    if sock.r#type()? != Type::STREAM {
        return Err(invalid("не SOCK_STREAM".into()));
    }
    let domain = sock.domain()?;
    if domain != Domain::IPV4 && domain != Domain::IPV6 {
        return Err(invalid(format!("семейство не IPv4/IPv6: {domain:?}")));
    }
    if !matches!(sock.protocol()?, None | Some(Protocol::TCP)) {
        return Err(invalid("протокол не TCP".into()));
    }
    sock.set_cloexec(true)?;
    claimed.insert(fd);
    // SAFETY: дескриптор открыт, это слушающий TCP-сокет, переданный процессу systemd; номер захвачен в
    // `claimed` (под блокировкой у `adopt_listen_fd`), поэтому владение создаётся один раз и `OwnedFd` —
    // единственный владелец, закрывающий дескриптор.
    #[allow(unsafe_code)]
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(std::net::TcpListener::from(owned))
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, IntoRawFd};

    use tokio::net::TcpStream;

    use super::*;

    fn localhost_v4() -> SocketAddr {
        (Ipv4Addr::LOCALHOST, 0).into()
    }

    #[test]
    fn section_from_yaml() {
        let cfg: ListenerConfig = serde_yaml_ng::from_str(
            "backlog: 128
reuse_port: false
",
        )
        .expect("yaml");
        assert_eq!(
            cfg,
            ListenerConfig {
                backlog: 128,
                reuse_port: false,
                ..ListenerConfig::default()
            }
        );
        let empty: ListenerConfig = serde_yaml_ng::from_str("{}").expect("empty section");
        assert_eq!(empty, ListenerConfig::default());
        let err = serde_yaml_ng::from_str::<ListenerConfig>(
            "backlg: 1
",
        )
        .expect_err("unknown");
        assert!(err.to_string().contains("unknown field `backlg`"), "{err}");
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

    /// Переменные systemd, в диапазон которых попадает `fd`.
    fn systemd_env(pid: u32, fd: RawFd) -> impl Fn(&str) -> Option<String> {
        move |name| match name {
            "LISTEN_PID" => Some(pid.to_string()),
            "LISTEN_FDS" => Some((fd - LISTEN_FDS_START + 1).to_string()),
            _ => None,
        }
    }

    #[test]
    fn adopted_fd_accepts_connections_once() {
        let listener = std::net::TcpListener::bind(localhost_v4()).expect("bind");
        let addr = listener.local_addr().expect("addr");
        // Владение дескриптором отдаётся — как у сокета, переданного systemd.
        let fd = listener.into_raw_fd();
        let pid = std::process::id();
        let mut claimed = BTreeSet::new();
        let adopted = adopt_in(fd, pid, systemd_env(pid, fd), &mut claimed).expect("adopt");
        assert_eq!(adopted.local_addr().expect("addr"), addr);

        let mut client = std::net::TcpStream::connect(addr).expect("connect");
        client.write_all(b"hi").expect("write");
        let (mut conn, _) = adopted.accept().expect("accept");
        let mut buf = [0u8; 2];
        conn.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"hi");

        let again = adopt_in(fd, pid, systemd_env(pid, fd), &mut claimed).expect_err("twice");
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        drop(adopted);
        // Номер закрыт, но остаётся захваченным.
        let closed = adopt_in(fd, pid, systemd_env(pid, fd), &mut claimed).expect_err("closed");
        assert_eq!(closed.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn rejects_foreign_or_unsuitable_fds() {
        let pid = std::process::id();
        let mut claimed = BTreeSet::new();

        // Сокет не слушает.
        let idle = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).expect("socket");
        let fd = idle.as_raw_fd();
        let e = adopt_in(fd, pid, systemd_env(pid, fd), &mut claimed).expect_err("not listening");
        assert!(e.to_string().contains("SO_ACCEPTCONN"), "{e}");

        // Датаграммный сокет.
        let udp = std::net::UdpSocket::bind(localhost_v4()).expect("udp");
        let fd = udp.as_raw_fd();
        assert!(adopt_in(fd, pid, systemd_env(pid, fd), &mut claimed).is_err());

        // Слушающий Unix-сокет — не TCP.
        let dir = std::env::temp_dir().join(format!("tp-sys-adopt-{pid}"));
        let _ = std::fs::remove_file(&dir);
        let unix = std::os::unix::net::UnixListener::bind(&dir).expect("unix");
        let fd = unix.as_raw_fd();
        let e = adopt_in(fd, pid, systemd_env(pid, fd), &mut claimed).expect_err("unix");
        assert!(e.to_string().contains("IPv4/IPv6"), "{e}");
        drop(unix);
        let _ = std::fs::remove_file(&dir);

        // Обычный файл.
        let file = std::fs::File::open("/proc/self/status").expect("file");
        let fd = file.as_raw_fd();
        let e = adopt_in(fd, pid, systemd_env(pid, fd), &mut claimed).expect_err("file");
        assert!(e.to_string().contains("не сокет"), "{e}");

        // Подходящий сокет, но переменные systemd не о нём.
        let tcp = std::net::TcpListener::bind(localhost_v4()).expect("bind");
        let fd = tcp.as_raw_fd();
        for env in [systemd_env(pid + 1, fd), systemd_env(pid, LISTEN_FDS_START)] {
            assert!(adopt_in(fd, pid, env, &mut claimed).is_err());
        }
        assert!(adopt_in(fd, pid, |_| None, &mut claimed).is_err());
        assert!(adopt_in(0, pid, systemd_env(pid, fd), &mut claimed).is_err());

        // Отказы ничего не захватили; исходные сокеты по-прежнему владеют дескрипторами.
        assert!(claimed.is_empty());
        drop(idle);
        tcp.local_addr().expect("still open");
    }
}
