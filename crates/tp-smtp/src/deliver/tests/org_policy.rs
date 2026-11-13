//! Политика организации в SMTP (SMTP-40, решение 97): лимиты сеанса submission (`max_recipients`,
//! `send_per_hour`) и профиль Milter — из цепочки «организация → домен → учётная запись», ключей config.yaml нет.

use std::collections::BTreeSet;

use base64::Engine as _;
use tp_contracts::auth::Principal;
use tp_contracts::meta::{Policy, PolicyStore, PolicyTarget};
use tp_platform::SectionRegistry;
use tp_testkit::{FakeAuthenticator, FakeSasl, MemPolicies};

use super::*;
use crate::config::MilterConfig;
use crate::milter::proto;
use crate::milter::tests::{Mock, on_eob, reply_of};

pub(super) const ALICE: &str = "alice@example.org";

/// Политика учётной записи alice (остальные уровни не заданы).
pub(super) async fn alice_policy(env: &Env, policy: Policy) -> MemPolicies {
    let policies = MemPolicies::new(&env.meta);
    let scope = env.scope(ALICE).await;
    policies
        .set_policy(scope.tenant, PolicyTarget::Account(scope.account), &policy)
        .await
        .expect("policy");
    policies
}

pub(super) async fn principal(env: &Env) -> Principal {
    let scope = env.scope(ALICE).await;
    Principal {
        tenant: scope.tenant,
        account: scope.account,
        login: ALICE.into(),
        groups: BTreeSet::new(),
        scopes: None,
    }
}

/// Сеанс submission по TLS с доставкой `d`; клиент уже вошёл как alice.
pub(super) async fn submission(
    env: &Env,
    d: Delivery,
) -> (
    Client<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<std::io::Result<crate::server::Exit>>,
) {
    let auth = FakeAuthenticator::default();
    auth.add_password(ALICE, "secret", principal(env).await);
    let ctx = SessionCtx {
        mode: Mode::Submission,
        config: Arc::new(SmtpConfig {
            hostname: "mx.example.org".into(),
            ..SmtpConfig::default()
        }),
        handler: Arc::new(d),
        sasl: Some(Arc::new(FakeSasl::new(Arc::new(auth)))),
        limiter: None,
        send_limits: Some(Arc::new(crate::SendLimits::new())),
        metrics: None,
        screen: None,
    };
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(async move {
        Session::new(ctx, conn_info(true), false)
            .serve(&mut server, true)
            .await
    });
    let mut c = Client::new(client);
    assert_eq!(c.code().await, 220);
    assert_eq!(c.cmd("EHLO client.example.net").await, 250);
    let plain = base64::engine::general_purpose::STANDARD.encode(format!("\0{ALICE}\0secret"));
    assert_eq!(c.cmd(&format!("AUTH PLAIN {plain}")).await, 235);
    (c, task)
}

/// Политика учётной записи с `max_recipients = 2`: третий получатель отклонён (`452 4.5.3`).
#[tokio::test]
async fn account_max_recipients() {
    let env = Env::new(DecisionHooks::default()).await;
    let policies = alice_policy(
        &env,
        Policy {
            max_recipients: Some(2),
            ..Policy::default()
        },
    )
    .await;
    let d = env.delivery(false).with_policies(Arc::new(policies));
    let (mut c, task) = submission(&env, d).await;
    assert_eq!(c.cmd(&format!("MAIL FROM:<{ALICE}>")).await, 250);
    assert_eq!(c.cmd("RCPT TO:<bob@example.org>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<carol@example.org>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<alice@example.org>").await, 452);
    assert_eq!(c.cmd("QUIT").await, 221);
    task.await.expect("join").expect("io");
}

/// Политика учётной записи с `send_per_hour = 1`: второе письмо за час — `451 4.7.1`.
#[tokio::test]
async fn account_send_per_hour() {
    let env = Env::new(DecisionHooks::default()).await;
    let policies = alice_policy(
        &env,
        Policy {
            send_per_hour: Some(1),
            ..Policy::default()
        },
    )
    .await;
    let d = env.delivery(false).with_policies(Arc::new(policies));
    let (mut c, task) = submission(&env, d).await;
    assert_eq!(c.cmd(&format!("MAIL FROM:<{ALICE}>")).await, 250);
    assert_eq!(c.cmd("RCPT TO:<bob@example.org>").await, 250);
    assert_eq!(c.cmd("DATA").await, 354);
    c.send("Subject: hi\r\n\r\nhello\r\n.\r\n").await;
    assert_eq!(c.code().await, 250);
    assert_eq!(c.cmd(&format!("MAIL FROM:<{ALICE}>")).await, 451);
    assert_eq!(c.cmd("QUIT").await, 221);
    task.await.expect("join").expect("io");
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
}

/// Профиль Milter из политики учётной записи отправителя вызывает свой набор фильтров: `antispam` — только
/// `rspamd`; без политики — оба.
#[tokio::test]
async fn milter_profile_from_policy() {
    for (profile, dlp_called) in [(Some("antispam"), false), (None, true)] {
        let env = Env::new(DecisionHooks::default()).await;
        let rspamd = Mock::start(0, on_eob(vec![reply_of(proto::R_ACCEPT)])).await;
        let dlp = Mock::start(0, on_eob(vec![reply_of(proto::R_ACCEPT)])).await;
        let mut profiles = std::collections::BTreeMap::new();
        profiles.insert("antispam".to_owned(), vec!["rspamd".to_owned()]);
        let submission = |mut f: crate::config::MilterFilter| {
            f.direction = crate::config::MilterDirection::Outbound;
            f
        };
        let milters = Milters::new(
            MilterConfig {
                filters: vec![
                    submission(rspamd.filter("rspamd")),
                    submission(dlp.filter("dlp")),
                ],
                profiles,
            },
            "mx.example.org",
        );
        let policies = alice_policy(
            &env,
            Policy {
                milter_profile: profile.map(str::to_owned),
                ..Policy::default()
            },
        )
        .await;
        let mut d = env.delivery(false).with_policies(Arc::new(policies));
        d.deps.milter = Some(Arc::new(milters));
        let mut e = envelope(Mode::Submission, &["bob@example.org"]);
        e.mail_from = ALICE.into();
        e.principal = Some(principal(&env).await);
        let r = d.data(&e, Bytes::from_static(OWN)).await;
        assert_eq!(r.code, 250, "{r:?}");
        assert!(!rspamd.codes().await.is_empty(), "{profile:?}");
        assert_eq!(!dlp.codes().await.is_empty(), dlp_called, "{profile:?}");
    }
}

/// Отправка вне сессии SMTP (JMAP): фильтры исходящей почты из профиля политики организации.
#[tokio::test]
async fn submit_milter_profile_from_tenant_policy() {
    let env = Env::new(DecisionHooks::default()).await;
    let rspamd = Mock::start(0, on_eob(vec![reply_of(proto::R_ACCEPT)])).await;
    let dlp = Mock::start(0, on_eob(vec![reply_of(proto::R_ACCEPT)])).await;
    let outbound = |mut f: crate::config::MilterFilter| {
        f.direction = crate::config::MilterDirection::Outbound;
        f
    };
    let mut profiles = std::collections::BTreeMap::new();
    profiles.insert("antispam".to_owned(), vec!["rspamd".to_owned()]);
    let milters = Milters::new(
        MilterConfig {
            filters: vec![
                outbound(rspamd.filter("rspamd")),
                outbound(dlp.filter("dlp")),
            ],
            profiles,
        },
        "mx.example.org",
    );
    let policies = MemPolicies::new(&env.meta);
    let policy = Policy {
        milter_profile: Some("antispam".into()),
        ..Policy::default()
    };
    policies
        .set_policy(TenantId::DEFAULT, PolicyTarget::Tenant, &policy)
        .await
        .expect("policy");
    let mut d = env.delivery(false).with_policies(Arc::new(policies));
    d.deps.milter = Some(Arc::new(milters));
    let sent = d
        .submit(
            TenantId::DEFAULT,
            None,
            ALICE,
            &["bob@example.org".to_owned()],
            Bytes::from_static(MAIL),
        )
        .await
        .expect("submit");
    assert_eq!(sent.rcpts[0].1, SubmitStatus::Delivered);
    assert!(!rspamd.codes().await.is_empty());
    assert_eq!(dlp.codes().await, "");
}

/// Ключи лимитов и селектора DKIM, которые теперь задаёт политика, — ошибка `thepost config check`.
#[test]
fn policy_keys_are_parse_errors() {
    let mut r = SectionRegistry::new();
    r.add_section::<SmtpConfig>("smtp").expect("smtp");
    let base = "database: { url: postgres://db/thepost }\nblob: { url: file:///var/lib/thepost }\n\
                domain: example.com\n";
    for (yaml, key) in [
        (
            "smtp:\n  limits:\n    max_recipients: 10\n",
            "smtp.limits.max_recipients",
        ),
        (
            "smtp:\n  limits:\n    messages_per_hour: 10\n",
            "smtp.limits.messages_per_hour",
        ),
        (
            "smtp:\n  limits:\n    recipients_per_hour: 10\n",
            "smtp.limits.recipients_per_hour",
        ),
        (
            "smtp:\n  dkim:\n    domains:\n      example.com:\n        selector: s1\n        keys: {}\n",
            "selector",
        ),
    ] {
        let err = r
            .parse_strict(&format!("{base}{yaml}"), Vec::new())
            .expect_err(key)
            .to_string();
        assert!(err.contains(key), "{key}: {err}");
    }
}
