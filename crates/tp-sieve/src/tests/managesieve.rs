//! Сеанс ManageSieve через `tokio::io::duplex` (SIEVE-08): приветствие, `STARTTLS`, вход, лимиты.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tp_contracts::auth::Principal;
use tp_contracts::meta::AccountScope;
use tp_contracts::sieve::SieveLimits;
use tp_contracts::tls::Profile;
use tp_contracts::{AccountId, Result, TenantId};
use tp_platform::listen::{EndpointConfig, Listener};
use tp_testkit::fakes::{FakeAuthenticator, FakeSasl};
use tp_testkit::sieve::MemSieve;

use crate::SieveConfig;
use crate::managesieve::{Deps, Session, SessionCtx, Transport, serve};

/// Соединение теста: «TLS» — признак, рукопожатия нет.
/// Адрес клиента соединений стенда.
const PEER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 7));

struct TestConn {
    io: DuplexStream,
    tls: bool,
    certs: bool,
}

impl Transport for TestConn {
    type Io = DuplexStream;

    fn io(&mut self) -> &mut DuplexStream {
        &mut self.io
    }

    fn tls(&self) -> bool {
        self.tls
    }

    fn peer(&self) -> IpAddr {
        PEER
    }

    fn can_starttls(&self) -> bool {
        self.certs && !self.tls
    }

    async fn starttls(self) -> Result<Self> {
        Ok(Self { tls: true, ..self })
    }
}

/// Стенд: сеанс над парой потоков, хранилище и учётная запись `user` / `secret` (токен `token`).
pub(super) struct Stand {
    io: DuplexStream,
    buf: Vec<u8>,
    pub(super) store: Arc<MemSieve>,
    pub(super) scope: AccountScope,
    /// Проверка учётных данных: адреса клиентов вызовов (CTR-57).
    pub(super) auth: FakeAuthenticator,
    pub(super) shutdown: CancellationToken,
    session: JoinHandle<()>,
}

pub(super) struct Setup {
    pub(super) tls: bool,
    pub(super) certs: bool,
    pub(super) profile: Profile,
    pub(super) config: SieveConfig,
    pub(super) limits: SieveLimits,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            tls: false,
            certs: true,
            profile: Profile::Prod,
            config: SieveConfig::default(),
            limits: SieveLimits::default(),
        }
    }
}

impl Setup {
    /// Соединение с неявным TLS.
    pub(super) fn tls() -> Self {
        Self {
            tls: true,
            ..Self::default()
        }
    }

    pub(super) fn start(self) -> Stand {
        let (tls, certs) = (self.tls, self.certs);
        let parts = self.parts();
        let (client, server) = tokio::io::duplex(1 << 20);
        let conn = TestConn {
            io: server,
            tls,
            certs,
        };
        Stand {
            io: client,
            buf: Vec::new(),
            store: parts.store,
            scope: parts.scope,
            auth: parts.auth,
            shutdown: parts.ctx.shutdown.clone(),
            session: tokio::spawn(Session::new(parts.ctx, conn).run()),
        }
    }

    /// Контекст сеансов стенда: хранилище и учётная запись `user` / `secret` (токен `token`).
    fn parts(self) -> Parts {
        let scope = AccountScope {
            tenant: TenantId::DEFAULT,
            account: AccountId::new(),
        };
        let principal = Principal {
            tenant: scope.tenant,
            account: scope.account,
            login: "user".into(),
            groups: BTreeSet::new(),
            scopes: None,
        };
        let auth = FakeAuthenticator::default();
        auth.add_password("user", "secret", principal.clone());
        auth.add_token("token", principal);
        let store = Arc::new(MemSieve::new().with_limits(self.limits));
        let ctx = Arc::new(SessionCtx {
            config: Arc::new(self.config),
            deps: Deps {
                sasl: Arc::new(FakeSasl::new(Arc::new(auth.clone()))),
                store: store.clone(),
            },
            profile: self.profile,
            shutdown: CancellationToken::new(),
        });
        Parts {
            ctx,
            store,
            scope,
            auth,
        }
    }
}

/// Части стенда: контекст сеансов и то, что тесты проверяют.
struct Parts {
    ctx: Arc<SessionCtx>,
    store: Arc<MemSieve>,
    scope: AccountScope,
    auth: FakeAuthenticator,
}

impl Stand {
    /// Ответ до завершающей строки `OK` / `NO` / `BYE` включительно.
    pub(super) async fn response(&mut self) -> String {
        let mut from = 0;
        loop {
            while let Some(len) = self.buf[from..].iter().position(|&b| b == b'\n') {
                let line = &self.buf[from..from + len];
                from += len + 1;
                if [&b"OK"[..], b"NO", b"BYE"]
                    .iter()
                    .any(|p| line.starts_with(p))
                {
                    // Текст ответа литералом `{n}`: ещё `n` байт и CRLF.
                    let literal = line
                        .strip_suffix(b"}\r")
                        .and_then(|l| l.rsplit(|&b| b == b'{').next())
                        .and_then(|n| std::str::from_utf8(n).ok()?.parse::<usize>().ok());
                    if let Some(n) = literal {
                        from += n + 2;
                        while self.buf.len() < from {
                            let n = self.io.read_buf(&mut self.buf).await.expect("чтение");
                            assert!(n > 0, "соединение закрыто");
                        }
                    }
                    let out: Vec<u8> = self.buf.drain(..from).collect();
                    return String::from_utf8(out).expect("utf8");
                }
            }
            let n = tokio::time::timeout(Duration::from_secs(10), self.io.read_buf(&mut self.buf))
                .await
                .expect("ответ сервера")
                .expect("чтение");
            assert!(
                n > 0,
                "соединение закрыто: {:?}",
                String::from_utf8_lossy(&self.buf)
            );
        }
    }

    /// Одна строка (вызов SASL).
    async fn line(&mut self) -> String {
        loop {
            if let Some(end) = self.buf.iter().position(|&b| b == b'\n') {
                let out: Vec<u8> = self.buf.drain(..=end).collect();
                return String::from_utf8(out).expect("utf8");
            }
            let n = self.io.read_buf(&mut self.buf).await.expect("чтение");
            assert!(n > 0, "соединение закрыто");
        }
    }

    pub(super) async fn send(&mut self, data: impl AsRef<[u8]>) {
        self.io.write_all(data.as_ref()).await.expect("запись");
    }

    /// Команда (CRLF добавляется) и ответ на неё.
    pub(super) async fn cmd(&mut self, command: &str) -> String {
        self.send(command).await;
        self.send("\r\n").await;
        self.response().await
    }

    /// Вход `PLAIN` с начальным ответом.
    pub(super) async fn login(&mut self) {
        let r = self
            .cmd(&format!("AUTHENTICATE \"PLAIN\" \"{}\"", plain("secret")))
            .await;
        assert_eq!(r, "OK \"Authenticated\"\r\n");
    }

    /// Сервер закрыл соединение, сеанс завершён.
    async fn closed(mut self) {
        let mut rest = Vec::new();
        self.io.read_to_end(&mut rest).await.expect("чтение");
        assert_eq!(String::from_utf8_lossy(&rest), "");
        self.session.await.expect("сеанс");
    }
}

fn plain(password: &str) -> String {
    STANDARD.encode(format!("\0user\0{password}"))
}

#[tokio::test]
async fn greeting() {
    let mut s = Setup::default().start();
    let greeting = s.response().await;
    assert_eq!(
        greeting,
        "\"IMPLEMENTATION\" \"The Post\"\r\n\
         \"SASL\" \"\"\r\n\
         \"SIEVE\" \"fileinto envelope imap4flags vacation body relational subaddress copy mailbox \
         encoded-character comparator-i;ascii-numeric\"\r\n\
         \"MAXREDIRECTS\" \"4\"\r\n\
         \"UNAUTHENTICATE\"\r\n\
         \"VERSION\" \"1.0\"\r\n\
         \"STARTTLS\"\r\n\
         OK \"ManageSieve ready\"\r\n"
    );
    // CAPABILITY повторяет возможности; имя команды — без учёта регистра.
    let caps = s.cmd("capability").await;
    assert_eq!(
        caps.strip_suffix("OK \"Capability completed\"\r\n"),
        greeting.strip_suffix("OK \"ManageSieve ready\"\r\n")
    );

    // Неявный TLS: механизмы объявлены, STARTTLS — нет.
    let mut s = Setup::tls().start();
    let greeting = s.response().await;
    assert!(
        greeting.contains("\"SASL\" \"PLAIN OAUTHBEARER\"\r\n"),
        "{greeting}"
    );
    assert!(!greeting.contains("STARTTLS"), "{greeting}");

    // Без сертификатов STARTTLS не предлагается.
    let mut s = Setup {
        certs: false,
        ..Setup::default()
    }
    .start();
    assert!(!s.response().await.contains("STARTTLS"));
    assert_eq!(
        s.cmd("STARTTLS").await,
        "NO \"STARTTLS is not available\"\r\n"
    );
}

#[tokio::test]
async fn starttls_then_login() {
    let mut s = Setup::default().start();
    s.response().await;
    // Без TLS вход запрещён.
    assert_eq!(
        s.cmd(&format!("AUTHENTICATE \"PLAIN\" \"{}\"", plain("secret")))
            .await,
        "NO (ENCRYPT-NEEDED) \"TLS is required to authenticate\"\r\n"
    );
    // Команда, отправленная вместе со STARTTLS открытым текстом, после рукопожатия не исполняется.
    s.send("STARTTLS\r\nNOOP \"injected\"\r\n").await;
    assert_eq!(s.response().await, "OK \"Begin TLS negotiation now\"\r\n");
    let caps = s.response().await;
    assert!(
        caps.contains("\"SASL\" \"PLAIN OAUTHBEARER\"\r\n"),
        "{caps}"
    );
    assert!(!caps.contains("STARTTLS"), "{caps}");
    assert!(
        caps.ends_with("OK \"TLS negotiation successful\"\r\n"),
        "{caps}"
    );
    assert_eq!(
        s.cmd("STARTTLS").await,
        "NO \"STARTTLS is not available\"\r\n"
    );

    s.login().await;
    assert_eq!(
        s.cmd(&format!("AUTHENTICATE \"PLAIN\" \"{}\"", plain("secret")))
            .await,
        "NO \"Already authenticated\"\r\n"
    );
    assert_eq!(
        s.cmd("NOOP \"injected?\"").await,
        "OK (TAG \"injected?\") \"Done\"\r\n"
    );
    assert_eq!(s.cmd("UNAUTHENTICATE").await, "OK \"Unauthenticated\"\r\n");
    assert_eq!(
        s.cmd("UNAUTHENTICATE").await,
        "NO \"Not authenticated\"\r\n"
    );
    assert_eq!(s.cmd("LOGOUT").await, "OK \"Logout completed\"\r\n");
    s.closed().await;
}

#[tokio::test]
async fn login_variants() {
    // Без начального ответа: пустой вызов, ответ строкой.
    let mut s = Setup::tls().start();
    s.response().await;
    s.send("AUTHENTICATE \"PLAIN\"\r\n").await;
    assert_eq!(s.line().await, "\"\"\r\n");
    assert_eq!(
        s.cmd(&format!("\"{}\"", plain("secret"))).await,
        "OK \"Authenticated\"\r\n"
    );

    // Ответ литералом; имя механизма — без учёта регистра.
    let mut s = Setup::tls().start();
    s.response().await;
    let p = plain("secret");
    assert_eq!(
        s.cmd(&format!("AUTHENTICATE \"plain\" {{{}+}}\r\n{p}", p.len()))
            .await,
        "OK \"Authenticated\"\r\n"
    );

    // OAUTHBEARER (RFC 7628).
    let mut s = Setup::tls().start();
    s.response().await;
    let token = STANDARD.encode("n,a=user,\x01auth=Bearer token\x01\x01");
    assert_eq!(
        s.cmd(&format!("AUTHENTICATE \"OAUTHBEARER\" \"{token}\""))
            .await,
        "OK \"Authenticated\"\r\n"
    );

    // Отмена клиентом, неизвестный механизм, неверный base64.
    let mut s = Setup::tls().start();
    s.response().await;
    s.send("AUTHENTICATE \"PLAIN\"\r\n").await;
    s.line().await;
    assert_eq!(s.cmd("\"*\"").await, "NO \"Authentication cancelled\"\r\n");
    assert_eq!(
        s.cmd("AUTHENTICATE \"CRAM-MD5\"").await,
        "NO \"Unsupported authentication mechanism\"\r\n"
    );
    assert_eq!(
        s.cmd("AUTHENTICATE \"PLAIN\" \"!!!\"").await,
        "NO \"Invalid base64\"\r\n"
    );
    assert_eq!(s.cmd("AUTHENTICATE").await, "NO \"Invalid arguments\"\r\n");

    // profile: dev — вход без TLS разрешён.
    let mut s = Setup {
        profile: Profile::Dev,
        ..Setup::default()
    }
    .start();
    assert!(
        s.response()
            .await
            .contains("\"SASL\" \"PLAIN OAUTHBEARER\"\r\n")
    );
    s.login().await;
}

#[tokio::test]
async fn wrong_password() {
    let mut s = Setup::tls().start();
    s.response().await;
    let bad = format!("AUTHENTICATE \"PLAIN\" \"{}\"", plain("wrong"));
    assert_eq!(s.cmd(&bad).await, "NO \"Authentication failed\"\r\n");
    // Сеанс остаётся невошедшим, верный пароль принимается.
    assert_eq!(
        s.cmd("UNAUTHENTICATE").await,
        "NO \"Not authenticated\"\r\n"
    );
    s.login().await;
    assert_eq!(s.cmd("UNAUTHENTICATE").await, "OK \"Unauthenticated\"\r\n");
    // Третья неудача за сеанс (sieve.managesieve.max_auth_failures) — закрытие.
    assert_eq!(s.cmd(&bad).await, "NO \"Authentication failed\"\r\n");
    assert_eq!(s.cmd(&bad).await, "NO \"Authentication failed\"\r\n");
    assert_eq!(
        s.response().await,
        "BYE \"Too many authentication failures\"\r\n"
    );
    s.closed().await;
}

#[tokio::test]
async fn long_literal_and_line() {
    let mut config = SieveConfig::default();
    config.managesieve.max_literal = 64;
    config.managesieve.max_line = 128;
    let mut s = Setup {
        config,
        ..Setup::tls()
    }
    .start();
    s.response().await;
    // Литерал больше лимита пропускается целиком, сеанс продолжается.
    let big = "x".repeat(100_000);
    assert_eq!(
        s.cmd(&format!("NOOP {{{}+}}\r\n{big}", big.len())).await,
        "NO (QUOTA/MAXSIZE) \"Literal is too large\"\r\n"
    );
    assert_eq!(
        s.cmd("NOOP {3+}\r\nabc").await,
        "OK (TAG \"abc\") \"Done\"\r\n"
    );
    // Лишние аргументы и мусор — ошибка синтаксиса, поток не рассинхронизирован.
    assert_eq!(
        s.cmd("NOOP {1+}\r\na {1+}\r\nb {1+}\r\nc {1+}\r\nd").await,
        "NO \"Syntax error\"\r\n"
    );
    assert_eq!(s.cmd("NOOP (x)").await, "NO \"Syntax error\"\r\n");
    assert_eq!(s.cmd("").await, "NO \"Command expected\"\r\n");
    assert_eq!(s.cmd("FROBNICATE").await, "NO \"Unknown command\"\r\n");
    // Строка длиннее лимита — закрытие.
    s.send(&"A".repeat(500)).await;
    assert_eq!(s.response().await, "BYE \"Line too long\"\r\n");
    s.closed().await;

    // Размер литерала вне диапазона — закрытие.
    let mut s = Setup::tls().start();
    s.response().await;
    assert_eq!(
        s.cmd("NOOP {99999999999+}").await,
        "BYE \"Invalid literal\"\r\n"
    );
    s.closed().await;
}

#[tokio::test]
async fn idle_timeout_and_shutdown() {
    let mut config = SieveConfig::default();
    config.managesieve.idle_timeout = Duration::from_millis(50);
    let mut s = Setup {
        config,
        ..Setup::tls()
    }
    .start();
    s.response().await;
    assert_eq!(s.response().await, "BYE \"Idle timeout\"\r\n");
    s.closed().await;

    let mut s = Setup::tls().start();
    s.response().await;
    s.shutdown.cancel();
    assert_eq!(s.response().await, "BYE \"Server is shutting down\"\r\n");
    s.closed().await;
}

/// SIEVE-12: `AUTHENTICATE PLAIN` — в проверку учётных данных приходит адрес клиента сессии, и при неудаче.
#[tokio::test]
async fn authenticate_passes_client_address() {
    let mut s = Setup::tls().start();
    s.response().await;
    let bad = format!("AUTHENTICATE \"PLAIN\" \"{}\"", plain("wrong"));
    assert_eq!(s.cmd(&bad).await, "NO \"Authentication failed\"\r\n");
    s.login().await;
    assert_eq!(s.auth.peers(), [Some(PEER); 2]);
}

/// Заголовок PROXY protocol v2 балансировщика (команда PROXY, TCP поверх IPv4): клиент `client` → `local`.
fn proxy_v2(client: SocketAddrV4, local: SocketAddrV4) -> Vec<u8> {
    // Сигнатура v2, затем версия 2 и команда PROXY, TCP поверх IPv4, длина адресов 12 байт.
    let mut h = vec![
        0x0d, 0x0a, 0x0d, 0x0a, 0x00, 0x0d, 0x0a, 0x51, 0x55, 0x49, 0x54, 0x0a, 0x21, 0x11, 0, 12,
    ];
    h.extend_from_slice(&client.ip().octets());
    h.extend_from_slice(&local.ip().octets());
    h.extend_from_slice(&client.port().to_be_bytes());
    h.extend_from_slice(&local.port().to_be_bytes());
    h
}

/// Строки ответа сервера до завершающей `OK` / `NO` / `BYE` включительно.
async fn tcp_response(r: &mut BufReader<OwnedReadHalf>) -> String {
    let mut out = String::new();
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(10), r.read_line(&mut line))
            .await
            .expect("ответ сервера")
            .expect("чтение");
        assert!(n > 0, "соединение закрыто: {out:?}");
        out.push_str(&line);
        if ["OK", "NO", "BYE"].iter().any(|p| line.starts_with(p)) {
            return out;
        }
    }
}

/// SIEVE-12: за балансировщиком с PROXY protocol в проверку учётных данных приходит адрес клиента из заголовка,
/// а не адрес балансировщика (соединение слушателя платформы, [`serve`]).
#[tokio::test]
async fn authenticate_behind_proxy_passes_client_address() {
    let parts = Setup {
        profile: Profile::Dev,
        ..Setup::default()
    }
    .parts();
    let tcp = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = tcp.local_addr().expect("addr");
    let mut ep = EndpointConfig::new(addr);
    ep.proxy_protocol = true;
    let listener = Listener::from_tcp(tcp, &ep, None).expect("listener");
    let server = tokio::spawn(serve(parts.ctx.clone(), listener, Duration::ZERO));

    let client: SocketAddrV4 = "203.0.113.7:4242".parse().expect("addr");
    let tcp = TcpStream::connect(addr).await.expect("connect");
    let (r, mut w) = tcp.into_split();
    let mut r = BufReader::new(r);
    w.write_all(&proxy_v2(client, "127.0.0.1:4190".parse().expect("addr")))
        .await
        .expect("proxy header");
    assert!(
        tcp_response(&mut r)
            .await
            .ends_with("OK \"ManageSieve ready\"\r\n")
    );
    w.write_all(format!("AUTHENTICATE \"PLAIN\" \"{}\"\r\n", plain("secret")).as_bytes())
        .await
        .expect("запись");
    assert_eq!(tcp_response(&mut r).await, "OK \"Authenticated\"\r\n");
    assert_eq!(
        parts.auth.peers(),
        [Some(IpAddr::V4(*client.ip()))],
        "адрес клиента из заголовка PROXY, не балансировщика"
    );

    parts.ctx.shutdown.cancel();
    server.await.expect("serve");
}
