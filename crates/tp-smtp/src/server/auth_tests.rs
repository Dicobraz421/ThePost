//! Перебор входа в SMTP AUTH (SMTP-41, решение 99): блокировки ведёт только защита от перебора за проверкой
//! учётных данных (`AbuseGuard` узла); сеанс после `limits.max_auth_failures` неудач подряд лишь закрывается,
//! адрес клиента в `RateLimiter` не блокируется.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
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
    async fn verify(&self, credentials: Credentials) -> Result<Principal> {
        if self.blocked.load(Ordering::SeqCst) {
            return Err(Error::Forbidden);
        }
        self.inner.verify(credentials).await
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
