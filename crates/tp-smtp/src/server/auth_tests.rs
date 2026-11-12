//! Перебор входа в SMTP AUTH (SMTP-41, решение 99): блокировки ведёт только защита от перебора за проверкой
//! учётных данных (`AbuseGuard` узла); сеанс после `limits.max_auth_failures` неудач подряд лишь закрывается,
//! адрес клиента в `RateLimiter` не блокируется. Адрес клиента сессии доходит до проверки учётных данных (SMTP-35).

use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tp_contracts::auth::{Authenticator, Credentials, Principal};
use tp_contracts::{Error, Result, TenantId};
use tp_platform::{RateLimitConfig, RateLimiter};
use tp_testkit::{FakeAuthenticator, FakeSasl, principal};

use super::Mode;
use super::tests::Client;
use super::tls_tests::{Server, ehlo, handshake, plain};

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Неудачные AUTH закрывают сеанс, но не блокируют адрес: новое соединение и вход с того же адреса проходят.
#[tokio::test]
async fn auth_failures_close_session_without_address_block() {
    let limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));
    let srv = Server::with_limiter(
        Mode::Submission,
        true,
        |c| c.limits.max_auth_failures = 2,
        Some(limiter.clone()),
    )
    .await;
    for _ in 0..3 {
        let mut c = Client::new(handshake(srv.connect().await).await);
        assert_eq!(c.code().await, 220);
        ehlo(&mut c).await;
        assert_eq!(c.cmd(&plain("wrong")).await, 535);
        assert_eq!(c.cmd(&plain("wrong")).await, 535);
        assert_eq!(c.code().await, 421);
        assert!(c.closed().await);
    }
    assert!(!limiter.is_blocked_at(LOCALHOST, Instant::now()));
    let mut c = Client::new(handshake(srv.connect().await).await);
    assert_eq!(c.code().await, 220);
    ehlo(&mut c).await;
    assert_eq!(c.cmd(&plain("secret")).await, 235);
    srv.stop().await;
}

/// Проверка учётных данных с блокировкой защиты от перебора (как `Guarded` узла поверх `AbuseGuard`):
/// заблокированная попытка — `Forbidden` без проверки пароля.
struct Blocking {
    inner: FakeAuthenticator,
    blocked: AtomicBool,
}

#[async_trait]
impl Authenticator for Blocking {
    async fn verify(&self, credentials: Credentials, peer: Option<IpAddr>) -> Result<Principal> {
        if self.blocked.load(Ordering::SeqCst) {
            return Err(Error::Forbidden);
        }
        self.inner.verify(credentials, peer).await
    }
}

/// Блокировка защиты от перебора отказывает во входе и верным паролем (`535`); без неё вход проходит.
#[tokio::test]
async fn abuse_block_refuses_login() {
    let inner = FakeAuthenticator::default();
    inner.add_password(
        "alice@example.org",
        "secret",
        principal(TenantId::DEFAULT, "alice@example.org"),
    );
    let auth = Arc::new(Blocking {
        inner,
        blocked: AtomicBool::new(true),
    });
    let sasl = Arc::new(FakeSasl::new(auth.clone()));
    let srv = Server::with_deps(Mode::Submission, true, |_| {}, |d| d.sasl = Some(sasl)).await;
    let mut c = Client::new(handshake(srv.connect().await).await);
    assert_eq!(c.code().await, 220);
    ehlo(&mut c).await;
    assert_eq!(c.cmd(&plain("secret")).await, 535);
    auth.blocked.store(false, Ordering::SeqCst);
    assert_eq!(c.cmd(&plain("secret")).await, 235);
    srv.stop().await;
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

/// SMTP-35: AUTH PLAIN — в проверку учётных данных приходит адрес клиента сессии (и при неудаче); за
/// балансировщиком с PROXY protocol — адрес клиента из заголовка, а не балансировщика.
#[tokio::test]
async fn auth_passes_client_address() {
    let auth = FakeAuthenticator::default();
    auth.add_password(
        "alice@example.org",
        "secret",
        principal(TenantId::DEFAULT, "alice@example.org"),
    );
    let sasl = Arc::new(FakeSasl::new(Arc::new(auth.clone())));
    let srv = Server::with_deps(
        Mode::Submission,
        true,
        |_| {},
        |d| d.sasl = Some(sasl.clone()),
    )
    .await;
    let mut c = Client::new(handshake(srv.connect().await).await);
    assert_eq!(c.code().await, 220);
    ehlo(&mut c).await;
    assert_eq!(c.cmd(&plain("wrong")).await, 535);
    assert_eq!(c.cmd(&plain("secret")).await, 235);
    assert_eq!(auth.peers(), [Some(LOCALHOST); 2]);
    srv.stop().await;

    let client: SocketAddrV4 = "203.0.113.7:4242".parse().expect("addr");
    let srv = Server::with_endpoint(
        Mode::Submission,
        |ep| {
            ep.implicit_tls = true;
            ep.proxy_protocol = true;
        },
        |_| {},
        |d| d.sasl = Some(sasl),
    )
    .await;
    let mut tcp = srv.connect().await;
    tcp.write_all(&proxy_v2(client, "127.0.0.1:465".parse().expect("addr")))
        .await
        .expect("proxy header");
    let mut c = Client::new(handshake(tcp).await);
    assert_eq!(c.code().await, 220);
    ehlo(&mut c).await;
    assert_eq!(c.cmd(&plain("secret")).await, 235);
    assert_eq!(
        auth.peers().last(),
        Some(&Some(IpAddr::V4(*client.ip()))),
        "адрес клиента из заголовка PROXY, не балансировщика"
    );
    srv.stop().await;
}
