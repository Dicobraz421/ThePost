//! Слушатели TCP (SRS 6.1, 6.2): приём соединений, PROXY protocol v2 от L4-балансировщика (адрес клиента
//! в [`ConnInfo`]), неявный TLS (465, 993, 995, 443) и апгрейд до TLS по STARTTLS (25, 587, 143).
//!
//! Цикл приёма не ждёт клиента: [`Listener::accept`] только принимает TCP, а чтение заголовка PROXY и
//! TLS-рукопожатие выполняет [`Incoming::establish`] в задаче соединения с таймаутом.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use ppp::v2::{Addresses, Command, Header, PROTOCOL_PREFIX};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;
use tp_contracts::Error;
use tp_contracts::config::duration;

use crate::tls::TlsCerts;

/// Параметры слушателя; встраивается в секцию крейта протокола (`imap.listeners`, `smtp.submission` и т. п.).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    /// Адрес и порт. Требует перезапуска.
    pub listen: SocketAddr,
    /// Неявный TLS: рукопожатие сразу после соединения. Иначе протокол может предложить STARTTLS.
    #[serde(default)]
    pub implicit_tls: bool,
    /// Соединения приходят от балансировщика с заголовком PROXY protocol v2.
    #[serde(default)]
    pub proxy_protocol: bool,
    /// Адреса и подсети балансировщиков (`10.0.0.0/8`, `2001:db8::5`), от которых принимается заголовок PROXY;
    /// соединения с других адресов считаются прямыми. Пусто — заголовок обязателен для всех.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub trusted_proxies: Vec<Cidr>,
    /// Время на заголовок PROXY и TLS-рукопожатие (`10s`).
    #[serde(default = "default_handshake_timeout", with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub handshake_timeout: Duration,
    /// Длина очереди принятых соединений. Требует перезапуска.
    #[serde(default = "default_backlog")]
    pub backlog: u32,
}

fn default_handshake_timeout() -> Duration {
    Duration::from_secs(10)
}

fn default_backlog() -> u32 {
    1024
}

impl EndpointConfig {
    /// Слушатель на `listen` с параметрами по умолчанию: без TLS и PROXY.
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            implicit_tls: false,
            proxy_protocol: false,
            trusted_proxies: Vec::new(),
            handshake_timeout: default_handshake_timeout(),
            backlog: default_backlog(),
        }
    }

    fn expects_proxy_header(&self, peer: IpAddr) -> bool {
        let peer = peer.to_canonical();
        self.proxy_protocol
            && (self.trusted_proxies.is_empty()
                || self.trusted_proxies.iter().any(|c| c.contains(peer)))
    }
}

/// Подсеть в записи CIDR (`192.0.2.0/24`); адрес без префикса — один узел.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Подсеть адреса `ip` с префиксом `prefix` (ограничивается 32 / 128); биты узла обнуляются.
    pub fn new(ip: IpAddr, prefix: u8) -> Self {
        let ip = ip.to_canonical();
        let prefix = match ip {
            IpAddr::V4(_) => prefix.min(32),
            IpAddr::V6(_) => prefix.min(128),
        };
        let network = match ip {
            IpAddr::V4(v4) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
                IpAddr::V4(Ipv4Addr::from(u32::from(v4) & mask))
            }
            IpAddr::V6(v6) => {
                let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
                IpAddr::V6(Ipv6Addr::from(u128::from(v6) & mask))
            }
        };
        Self { network, prefix }
    }

    /// Адрес сети.
    pub fn network(&self) -> IpAddr {
        self.network
    }

    /// Длина префикса.
    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// Адрес входит в подсеть.
    pub fn contains(&self, ip: IpAddr) -> bool {
        Cidr::new(ip, self.prefix).network == self.network
    }
}

impl FromStr for Cidr {
    type Err = Error;

    fn from_str(s: &str) -> tp_contracts::Result<Self> {
        let bad = || Error::Invalid(format!("invalid CIDR: {s}"));
        let (addr, prefix) = match s.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (s, None),
        };
        let ip: IpAddr = addr.trim().parse().map_err(|_| bad())?;
        let max = if ip.to_canonical().is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(bad)?,
            None => max,
        };
        Ok(Cidr::new(ip, prefix))
    }
}

impl TryFrom<String> for Cidr {
    type Error = Error;

    fn try_from(s: String) -> tp_contracts::Result<Self> {
        s.parse()
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

#[derive(Debug, thiserror::Error)]
enum ListenError {
    #[error("connection: {0}")]
    Io(#[from] io::Error),
    #[error("handshake timed out")]
    Timeout,
    #[error("proxy protocol: {0}")]
    Proxy(String),
    #[error("tls handshake: {0}")]
    Tls(io::Error),
    #[error("tls is not configured for this listener")]
    NoTls,
    #[error("tls is already active")]
    AlreadyTls,
}

impl From<ListenError> for Error {
    fn from(e: ListenError) -> Self {
        Error::Invalid(e.to_string())
    }
}

/// Слушающий сокет протокола.
pub struct Listener {
    tcp: TcpListener,
    cfg: Arc<EndpointConfig>,
    tls: Option<Arc<TlsCerts>>,
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listener")
            .field("listen", &self.cfg.listen)
            .field("implicit_tls", &self.cfg.implicit_tls)
            .field("proxy_protocol", &self.cfg.proxy_protocol)
            .finish_non_exhaustive()
    }
}

impl Listener {
    /// Открывает сокет по `cfg`. `tls` — сертификаты для неявного TLS и STARTTLS. Требует контекста tokio-рантайма.
    pub fn bind(cfg: &EndpointConfig, tls: Option<Arc<TlsCerts>>) -> tp_contracts::Result<Self> {
        let socket = tp_sys::ListenerConfig {
            backlog: cfg.backlog,
            ..tp_sys::ListenerConfig::default()
        };
        let tcp = tp_sys::bind_tcp(cfg.listen, &socket)
            .map_err(|e| Error::Invalid(format!("listen {}: {e}", cfg.listen)))?;
        Self::from_tcp(tcp, cfg, tls)
    }

    /// Слушатель на готовом сокете (передача сокета systemd, тесты).
    pub fn from_tcp(
        tcp: TcpListener,
        cfg: &EndpointConfig,
        tls: Option<Arc<TlsCerts>>,
    ) -> tp_contracts::Result<Self> {
        if cfg.implicit_tls && tls.is_none() {
            return Err(Error::Invalid(format!(
                "listen {}: implicit_tls requires tls certificates",
                cfg.listen
            )));
        }
        Ok(Self {
            tcp,
            cfg: Arc::new(cfg.clone()),
            tls,
        })
    }

    /// Фактический адрес сокета.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.tcp.local_addr()
    }

    /// Принимает TCP-соединение. Рукопожатие — [`Incoming::establish`] в задаче соединения.
    pub async fn accept(&self) -> io::Result<Incoming> {
        let (tcp, peer) = self.tcp.accept().await?;
        let local = tcp.local_addr()?;
        Ok(Incoming {
            tcp,
            peer,
            local,
            cfg: self.cfg.clone(),
            tls: self.tls.clone(),
        })
    }
}

/// Принятое соединение до разбора PROXY и TLS-рукопожатия.
pub struct Incoming {
    tcp: TcpStream,
    peer: SocketAddr,
    local: SocketAddr,
    cfg: Arc<EndpointConfig>,
    tls: Option<Arc<TlsCerts>>,
}

impl fmt::Debug for Incoming {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Incoming")
            .field("peer", &self.peer)
            .field("local", &self.local)
            .finish_non_exhaustive()
    }
}

impl Incoming {
    /// Адрес TCP-собеседника (балансировщик при PROXY protocol).
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// Читает заголовок PROXY (если ожидается) и выполняет неявный TLS за `handshake_timeout`.
    pub async fn establish(self) -> tp_contracts::Result<Conn> {
        let timeout = self.cfg.handshake_timeout;
        Ok(tokio::time::timeout(timeout, self.establish_inner())
            .await
            .map_err(|_| ListenError::Timeout)??)
    }

    async fn establish_inner(mut self) -> Result<Conn, ListenError> {
        let mut info = ConnInfo {
            client: self.peer,
            local: self.local,
            proxy: None,
            tls: false,
        };
        if self.cfg.expects_proxy_header(self.peer.ip()) {
            if let Some((client, local)) = read_proxy_header(&mut self.tcp).await? {
                info.client = client;
                info.local = local;
            }
            info.proxy = Some(self.peer);
        }
        let conn = Conn {
            info,
            stream: Stream::Plain(self.tcp),
            tls: self.tls,
            timeout: self.cfg.handshake_timeout,
        };
        if self.cfg.implicit_tls {
            return conn.upgrade().await;
        }
        Ok(conn)
    }
}

/// Сведения о соединении для протокола, журналов и rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnInfo {
    /// Адрес клиента: из заголовка PROXY или адрес TCP-собеседника.
    pub client: SocketAddr,
    /// Адрес, на который обратился клиент (из заголовка PROXY, если он есть).
    pub local: SocketAddr,
    /// Адрес балансировщика, приславшего заголовок PROXY.
    pub proxy: Option<SocketAddr>,
    /// Канал защищён TLS.
    pub tls: bool,
}

/// Установленное соединение: TCP или TLS поверх TCP.
pub struct Conn {
    /// Сведения о соединении.
    pub info: ConnInfo,
    /// Поток данных.
    pub stream: Stream,
    tls: Option<Arc<TlsCerts>>,
    timeout: Duration,
}

impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conn")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl Conn {
    /// Протокол может предложить STARTTLS: сертификаты есть, канал ещё не защищён.
    pub fn can_starttls(&self) -> bool {
        self.tls.is_some() && !self.info.tls
    }

    /// Апгрейд до TLS после ответа клиенту на STARTTLS. Протокол обязан отбросить свой буфер чтения:
    /// данные, пришедшие до рукопожатия открытым текстом, не должны исполняться после него.
    pub async fn starttls(self) -> tp_contracts::Result<Conn> {
        let timeout = self.timeout;
        Ok(tokio::time::timeout(timeout, self.upgrade())
            .await
            .map_err(|_| ListenError::Timeout)??)
    }

    async fn upgrade(self) -> Result<Conn, ListenError> {
        let certs = self.tls.clone().ok_or(ListenError::NoTls)?;
        let tcp = match self.stream {
            Stream::Plain(tcp) => tcp,
            Stream::Tls(_) => return Err(ListenError::AlreadyTls),
        };
        let tls = certs.accept(tcp).await.map_err(ListenError::Tls)?;
        Ok(Conn {
            info: ConnInfo {
                tls: true,
                ..self.info
            },
            stream: Stream::Tls(Box::new(tls)),
            tls: self.tls,
            timeout: self.timeout,
        })
    }
}

/// Поток соединения.
#[derive(Debug)]
pub enum Stream {
    /// Открытый TCP.
    Plain(TcpStream),
    /// TLS поверх TCP.
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Читает заголовок PROXY v2 ровно по его длине (данные протокола за ним остаются в сокете).
/// `Some((клиент, адрес назначения))` — команда PROXY с адресами TCP; `None` — LOCAL (проверка балансировщика)
/// или адреса не IP.
async fn read_proxy_header(
    tcp: &mut TcpStream,
) -> Result<Option<(SocketAddr, SocketAddr)>, ListenError> {
    let mut head = [0u8; 16];
    tcp.read_exact(&mut head).await?;
    if head[..PROTOCOL_PREFIX.len()] != *PROTOCOL_PREFIX {
        return Err(ListenError::Proxy("missing v2 signature".to_owned()));
    }
    let len = usize::from(u16::from_be_bytes([head[14], head[15]]));
    let mut buf = vec![0u8; head.len() + len];
    buf[..head.len()].copy_from_slice(&head);
    tcp.read_exact(&mut buf[head.len()..]).await?;
    let header = Header::try_from(buf.as_slice()).map_err(|e| ListenError::Proxy(e.to_string()))?;
    if header.command == Command::Local {
        return Ok(None);
    }
    Ok(match header.addresses {
        Addresses::IPv4(a) => Some((
            SocketAddr::new(a.source_address.into(), a.source_port),
            SocketAddr::new(a.destination_address.into(), a.destination_port),
        )),
        Addresses::IPv6(a) => Some((
            SocketAddr::new(a.source_address.into(), a.source_port),
            SocketAddr::new(a.destination_address.into(), a.destination_port),
        )),
        Addresses::Unspecified | Addresses::Unix(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use ppp::v2::{Builder, Protocol, Version};
    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;
    use crate::tls::tests::{CERT_A, KEY_A, TempDir, connector, write_pair};

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("addr")
    }

    fn proxy_header(command: Command, src: &str, dst: &str) -> Vec<u8> {
        Builder::with_addresses(
            Version::Two | command,
            Protocol::Stream,
            (addr(src), addr(dst)),
        )
        .build()
        .expect("header")
    }

    async fn listener(cfg: EndpointConfig, tls: Option<Arc<TlsCerts>>) -> (Listener, SocketAddr) {
        let tcp = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let at = tcp.local_addr().expect("addr");
        (Listener::from_tcp(tcp, &cfg, tls).expect("listener"), at)
    }

    /// Принимает одно соединение, устанавливает его и возвращает сведения и первые 5 байт протокола.
    async fn serve_one(l: Listener) -> tp_contracts::Result<(ConnInfo, [u8; 5])> {
        let conn = l.accept().await.expect("accept").establish().await?;
        let info = conn.info;
        let mut stream = conn.stream;
        let mut buf = [0u8; 5];
        stream.read_exact(&mut buf).await.expect("payload");
        Ok((info, buf))
    }

    #[tokio::test]
    async fn proxy_v2_header_gives_client_address() {
        let mut cfg = EndpointConfig::new(addr("127.0.0.1:0"));
        cfg.proxy_protocol = true;
        let (l, at) = listener(cfg, None).await;
        let server = tokio::spawn(serve_one(l));
        let mut c = TcpStream::connect(at).await.expect("connect");
        let mut data = proxy_header(Command::Proxy, "203.0.113.7:4242", "198.51.100.1:993");
        data.extend_from_slice(b"hello");
        c.write_all(&data).await.expect("write");
        let (info, payload) = server.await.expect("join").expect("establish");
        assert_eq!(info.client, addr("203.0.113.7:4242"));
        assert_eq!(info.local, addr("198.51.100.1:993"));
        assert_eq!(info.proxy, Some(c.local_addr().expect("addr")));
        assert!(!info.tls);
        assert_eq!(&payload, b"hello", "protocol bytes after header are kept");
    }

    #[tokio::test]
    async fn proxy_v2_ipv6_local_and_garbage() {
        let mut cfg = EndpointConfig::new(addr("127.0.0.1:0"));
        cfg.proxy_protocol = true;

        let (l, at) = listener(cfg.clone(), None).await;
        let server = tokio::spawn(serve_one(l));
        let mut c = TcpStream::connect(at).await.expect("connect");
        let mut data = proxy_header(Command::Proxy, "[2001:db8::7]:5000", "[2001:db8::1]:443");
        data.extend_from_slice(b"world");
        c.write_all(&data).await.expect("write");
        let (info, _) = server.await.expect("join").expect("establish");
        assert_eq!(info.client, addr("[2001:db8::7]:5000"));

        // LOCAL — проверка доступности от балансировщика: адрес клиента — сам балансировщик.
        let (l, at) = listener(cfg.clone(), None).await;
        let server = tokio::spawn(serve_one(l));
        let mut c = TcpStream::connect(at).await.expect("connect");
        let mut data = proxy_header(Command::Local, "10.0.0.1:1", "10.0.0.2:2");
        data.extend_from_slice(b"probe");
        c.write_all(&data).await.expect("write");
        let (info, _) = server.await.expect("join").expect("establish");
        assert_eq!(info.client, c.local_addr().expect("addr"));

        let (l, at) = listener(cfg, None).await;
        let server =
            tokio::spawn(async move { l.accept().await.expect("accept").establish().await });
        let mut c = TcpStream::connect(at).await.expect("connect");
        c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        let err = server.await.expect("join").expect_err("no header");
        assert!(err.to_string().contains("proxy protocol"), "{err}");
    }

    #[tokio::test]
    async fn untrusted_peer_is_direct_and_slow_peer_times_out() {
        let mut cfg = EndpointConfig::new(addr("127.0.0.1:0"));
        cfg.proxy_protocol = true;
        cfg.trusted_proxies = vec!["10.0.0.0/8".parse().expect("cidr")];
        let (l, at) = listener(cfg.clone(), None).await;
        let server = tokio::spawn(serve_one(l));
        let mut c = TcpStream::connect(at).await.expect("connect");
        c.write_all(b"plain").await.expect("write");
        let (info, payload) = server.await.expect("join").expect("establish");
        assert_eq!(info.client, c.local_addr().expect("addr"));
        assert_eq!(info.proxy, None);
        assert_eq!(&payload, b"plain");

        cfg.trusted_proxies = vec!["127.0.0.1".parse().expect("cidr")];
        cfg.handshake_timeout = Duration::from_millis(50);
        let (l, at) = listener(cfg, None).await;
        let server =
            tokio::spawn(async move { l.accept().await.expect("accept").establish().await });
        let _c = TcpStream::connect(at).await.expect("connect");
        let err = server.await.expect("join").expect_err("timeout");
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn starttls_upgrade_with_self_signed_cert() {
        let dir = TempDir::new("starttls");
        let certs = Arc::new(
            TlsCerts::load(&write_pair(&dir.0, CERT_A, KEY_A))
                .await
                .expect("certs"),
        );
        let (l, at) = listener(EndpointConfig::new(addr("127.0.0.1:0")), Some(certs)).await;
        let server = tokio::spawn(async move {
            let conn = l
                .accept()
                .await
                .expect("accept")
                .establish()
                .await
                .expect("establish");
            assert!(conn.can_starttls());
            let Conn {
                info,
                stream,
                tls,
                timeout,
            } = conn;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("command");
            assert_eq!(line, "STARTTLS\r\n");
            // Буфер чтения отбрасывается: дальше — только TLS.
            let mut stream = reader.into_inner();
            stream.write_all(b"OK\r\n").await.expect("reply");
            let conn = Conn {
                info,
                stream,
                tls,
                timeout,
            }
            .starttls()
            .await
            .expect("upgrade");
            assert!(conn.info.tls && !conn.can_starttls());
            let mut stream = conn.stream;
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).await.expect("ping");
            stream.write_all(b"pong").await.expect("pong");
            stream.shutdown().await.expect("shutdown");
            buf
        });
        let mut c = TcpStream::connect(at).await.expect("connect");
        c.write_all(b"STARTTLS\r\n").await.expect("write");
        let mut ok = [0u8; 4];
        c.read_exact(&mut ok).await.expect("ok");
        assert_eq!(&ok, b"OK\r\n");
        let name = ServerName::try_from("localhost").expect("name");
        let mut tls = connector(&[CERT_A], &[&rustls::version::TLS13])
            .connect(name, c)
            .await
            .expect("tls");
        tls.write_all(b"ping").await.expect("write");
        let mut buf = [0u8; 4];
        tls.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"pong");
        assert_eq!(&server.await.expect("join"), b"ping");
    }

    #[tokio::test]
    async fn implicit_tls_requires_certs_and_handshakes() {
        let mut cfg = EndpointConfig::new(addr("127.0.0.1:0"));
        cfg.implicit_tls = true;
        let tcp = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        assert!(Listener::from_tcp(tcp, &cfg, None).is_err());

        let dir = TempDir::new("implicit");
        let certs = Arc::new(
            TlsCerts::load(&write_pair(&dir.0, CERT_A, KEY_A))
                .await
                .expect("certs"),
        );
        let (l, at) = listener(cfg, Some(certs)).await;
        let server = tokio::spawn(serve_one(l));
        let c = TcpStream::connect(at).await.expect("connect");
        let name = ServerName::try_from("localhost").expect("name");
        let mut tls = connector(&[CERT_A], &[&rustls::version::TLS13])
            .connect(name, c)
            .await
            .expect("tls");
        tls.write_all(b"hello").await.expect("write");
        let (info, payload) = server.await.expect("join").expect("establish");
        assert!(info.tls);
        assert_eq!(&payload, b"hello");
    }

    #[test]
    fn cidr_parse_and_contains() {
        let net: Cidr = "192.0.2.77/24".parse().expect("v4");
        assert_eq!(net.to_string(), "192.0.2.0/24");
        assert!(net.contains("192.0.2.1".parse().expect("ip")));
        assert!(net.contains("::ffff:192.0.2.9".parse().expect("mapped")));
        assert!(!net.contains("192.0.3.1".parse().expect("ip")));
        let host: Cidr = "2001:db8::5".parse().expect("v6 host");
        assert_eq!(host.prefix(), 128);
        assert!(host.contains("2001:db8::5".parse().expect("ip")));
        let v6: Cidr = "2001:db8:1:2:3::/64".parse().expect("v6");
        assert_eq!(
            v6.network(),
            "2001:db8:1:2::".parse::<IpAddr>().expect("ip")
        );
        assert!("10.0.0.0/33".parse::<Cidr>().is_err());
        assert!("nonsense/8".parse::<Cidr>().is_err());
        let all: Cidr = "0.0.0.0/0".parse().expect("any");
        assert!(all.contains("8.8.8.8".parse().expect("ip")));
    }

    #[test]
    fn endpoint_config_from_yaml() {
        let cfg: EndpointConfig = serde_yaml_ng::from_str(
            "listen: \"[::]:993\"\nimplicit_tls: true\nproxy_protocol: true\ntrusted_proxies: [10.0.0.0/8]\n",
        )
        .expect("parse");
        assert!(cfg.implicit_tls && cfg.proxy_protocol);
        assert_eq!(cfg.trusted_proxies[0].to_string(), "10.0.0.0/8");
        assert_eq!(cfg.handshake_timeout, Duration::from_secs(10));
        let fast: EndpointConfig =
            serde_yaml_ng::from_str("listen: \"0.0.0.0:1\"\nhandshake_timeout: 250ms")
                .expect("parse");
        assert_eq!(fast.handshake_timeout, Duration::from_millis(250));
        for bad in ["handshake_timeout: 10000", "handshake_timeout_ms: 10000"] {
            let text = format!("listen: \"0.0.0.0:1\"\n{bad}");
            assert!(
                serde_yaml_ng::from_str::<EndpointConfig>(&text).is_err(),
                "{bad}"
            );
        }
        assert!(
            serde_yaml_ng::from_str::<EndpointConfig>("implicit_tls: true").is_err(),
            "listen is required"
        );
        assert!(
            serde_yaml_ng::from_str::<EndpointConfig>(
                "listen: \"0.0.0.0:1\"\ntrusted_proxies: [bad]"
            )
            .is_err()
        );
    }
}
