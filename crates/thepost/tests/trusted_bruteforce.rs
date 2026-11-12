//! PLT-32: адрес из `ratelimit.trusted` не ограничивается по частоте соединений, но перебор паролей с него
//! блокируется защитой от перебора (`auth.abuse`, [`Guarded`]): после порога неудач отвергается и верный пароль.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Arc;

use thepost::services::Guarded;
use tp_contracts::auth::{Authenticator, Credentials, Principal};
use tp_contracts::{Error, TenantId};
use tp_platform::{RateLimitConfig, RateLimiter};
use tp_testkit::{FakeAuthenticator, MemAbuseBlocks, MemMetadata, SystemClock};

const LOGIN: &str = "alice@example.org";

fn password(secret: &str) -> Credentials {
    Credentials::Password {
        login: LOGIN.into(),
        secret: secret.into(),
    }
}

/// Проверка паролей под защитой от перебора: блокировка учётной записи после `account_failures` неудач.
async fn guarded(account_failures: u32) -> Guarded {
    let meta = MemMetadata::new(Arc::new(SystemClock));
    let scope = meta.account(LOGIN).await.expect("account");
    let fake = FakeAuthenticator::default();
    fake.add_password(
        LOGIN,
        "right",
        Principal {
            tenant: scope.tenant,
            account: scope.account,
            login: LOGIN.into(),
            groups: BTreeSet::new(),
            scopes: None,
        },
    );
    let cfg = tp_auth::AbuseConfig {
        account_failures,
        ..tp_auth::AbuseConfig::default()
    };
    Guarded::new(
        Arc::new(fake),
        tp_auth::AbuseGuard::new(cfg, Arc::new(MemAbuseBlocks::default())),
        Arc::new(meta),
        TenantId::DEFAULT,
    )
}

#[tokio::test]
async fn bruteforce_from_trusted_source_is_still_blocked() {
    let trusted = IpAddr::from([192, 0, 2, 10]);
    let stranger = IpAddr::from([198, 51, 100, 7]);
    let limiter = RateLimiter::new(RateLimitConfig {
        trusted: vec!["192.0.2.0/24".parse().expect("cidr")],
        ..RateLimitConfig::default()
    });
    let burst = RateLimitConfig::default().ip.burst;

    // Частота соединений: недоверенный адрес упирается в лимит адреса, доверенный — нет.
    for _ in 0..burst {
        assert!(limiter.check_ip(stranger).is_allowed());
    }
    assert!(!limiter.check_ip(stranger).is_allowed());
    for _ in 0..burst * 2 {
        assert!(limiter.check_ip(trusted).is_allowed());
    }

    // Перебор паролей с доверенного адреса: соединения пропускаются, вход блокируется по порогу неудач.
    let g = guarded(3).await;
    for _ in 0..3 {
        assert!(limiter.check_ip(trusted).is_allowed());
        assert_eq!(g.verify(password("wrong")).await, Err(Error::Forbidden));
    }
    assert!(limiter.check_ip(trusted).is_allowed());
    assert_eq!(g.verify(password("right")).await, Err(Error::Forbidden));
}
