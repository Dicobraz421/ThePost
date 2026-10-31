//! SPF, DKIM, DMARC и ARC на подписанных фикстурах: ключи Ed25519 создаются в тесте, записи DNS — в кэше
//! проверок вместо сети (мок-резолвер: резолвер без серверов, все ответы — из кэша).

use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use mail_auth::arc::ArcSealer;
use mail_auth::common::crypto::Ed25519Key;
use mail_auth::common::parse::TxtRecordParser;
use mail_auth::common::verify::DomainKey;
use mail_auth::dkim::DkimSigner;
use mail_auth::dmarc::Dmarc;
use mail_auth::hickory_resolver::config::{ResolverConfig, ResolverOpts};
use mail_auth::spf::Spf;
use time::OffsetDateTime;

use super::*;
use crate::server::Mode;
use crate::server::tests::conn_info;

/// Ключ подписи домена и его запись `_domainkey`.
pub(crate) struct Signer {
    pkcs8: Vec<u8>,
    domain: &'static str,
    selector: &'static str,
}

impl Signer {
    pub(crate) fn new(domain: &'static str, selector: &'static str) -> Self {
        Self {
            pkcs8: Ed25519Key::generate_pkcs8().expect("key"),
            domain,
            selector,
        }
    }

    fn key(&self) -> Ed25519Key {
        Ed25519Key::from_pkcs8_der(&self.pkcs8).expect("key")
    }

    /// Публикует открытый ключ в кэше DNS.
    pub(crate) fn publish(&self, cache: &DnsCache) {
        let record = format!(
            "v=DKIM1; k=ed25519; p={}",
            B64.encode(self.key().public_key())
        );
        txt(
            cache,
            &format!("{}._domainkey.{}.", self.selector, self.domain),
            Txt::from(DomainKey::parse(record.as_bytes())),
        );
    }

    /// Письмо с подписью DKIM.
    pub(crate) fn sign(&self, raw: &str) -> String {
        let sig = DkimSigner::from_key(self.key())
            .domain(self.domain)
            .selector(self.selector)
            .headers(["From", "To", "Subject"])
            .sign(raw.as_bytes())
            .expect("sign");
        format!("{}{raw}", sig.to_header())
    }
}

fn txt(cache: &DnsCache, name: &str, value: Txt) {
    cache.txt.insert(
        name.into(),
        value,
        Instant::now() + Duration::from_secs(3600),
    );
}

/// Проверки с резолвером без серверов: всё — из кэша.
pub(crate) fn checker() -> AuthCheck {
    let resolver = MessageAuthenticator::new(
        ResolverConfig::from_name_servers(vec![]),
        ResolverOpts::default(),
    )
    .expect("resolver");
    let cfg = SmtpConfig {
        hostname: "mx.example.org".into(),
        ..SmtpConfig::default()
    };
    AuthCheck::new(resolver, &cfg)
}

/// Домен отправителя `example.com`: SPF — `192.0.2.0/24`, DMARC — политика `policy`.
pub(crate) fn publish_sender(cache: &DnsCache, policy: &str) {
    let spf = Txt::from(Spf::parse(b"v=spf1 ip4:192.0.2.0/24 -all"));
    txt(cache, "example.com.", spf);
    let dmarc = format!("v=DMARC1; p={policy}");
    txt(
        cache,
        "_dmarc.example.com.",
        Txt::from(Dmarc::parse(dmarc.as_bytes())),
    );
    // Обход дерева DMARC (DMARCbis) спрашивает и `_dmarc.com.`: записи нет.
    let none = Txt::Error(mail_auth::Error::Dns(
        mail_auth::DnsError::InvalidRecordType,
    ));
    txt(cache, "_dmarc.com.", none);
}

pub(crate) const MAIL: &str =
    "From: sender@example.com\r\nTo: bob@example.org\r\nSubject: hello\r\n\r\nbody\r\n";

pub(crate) fn envelope(client: &str) -> Envelope {
    let mut conn = conn_info(false);
    conn.client = client.parse().expect("addr");
    Envelope {
        id: "t1".into(),
        mode: Mode::Inbound,
        conn,
        helo: "example.com".into(),
        esmtp: true,
        principal: None,
        mail_from: "sender@example.com".into(),
        rcpts: vec!["bob@example.org".into()],
        smtputf8: false,
        started_at: OffsetDateTime::now_utc(),
    }
}

#[tokio::test]
async fn signed_mail_passes() {
    let check = checker();
    let signer = Signer::new("example.com", "s1");
    signer.publish(check.cache());
    publish_sender(check.cache(), "reject");
    let raw = signer.sign(MAIL);
    let out = check
        .check(&envelope("192.0.2.10:1000"), raw.as_bytes())
        .await;
    assert!(!out.junk);
    let h = &out.headers;
    assert!(
        h.starts_with("Authentication-Results: mx.example.org;"),
        "{h}"
    );
    for want in [
        "dkim=pass",
        "spf=pass",
        "dmarc=pass",
        "arc=none",
        "header.d=example.com",
    ] {
        assert!(h.contains(want), "{want} in {h}");
    }
    assert!(h.contains("\r\nReceived-SPF: pass"), "{h}");
    assert!(h.ends_with("\r\n"));
}

#[tokio::test]
async fn dmarc_failure_with_reject_or_quarantine_goes_to_junk() {
    for (policy, junk) in [("reject", true), ("quarantine", true), ("none", false)] {
        let check = checker();
        let signer = Signer::new("example.com", "s1");
        signer.publish(check.cache());
        publish_sender(check.cache(), policy);
        // Тело изменено после подписи, адрес клиента вне SPF: DKIM и SPF не проходят.
        let raw = signer.sign(MAIL).replace("body", "tampered");
        let out = check
            .check(&envelope("203.0.113.5:1000"), raw.as_bytes())
            .await;
        assert_eq!(out.junk, junk, "p={policy}");
        let h = &out.headers;
        // Хэш тела не сошёлся — neutral (RFC 6376, 6.1.3).
        for want in ["dkim=neutral", "spf=fail", "dmarc=fail"] {
            assert!(h.contains(want), "{want} in {h}");
        }
        assert!(h.contains(&format!("policy.dmarc={policy}")), "{h}");
    }
}

#[tokio::test]
async fn arc_chain_of_forwarder_verifies() {
    let check = checker();
    let author = Signer::new("example.com", "s1");
    author.publish(check.cache());
    publish_sender(check.cache(), "reject");
    let forwarder = Signer::new("forwarder.example", "arc");
    forwarder.publish(check.cache());
    // Пересылающий узел проверил письмо и запечатал результат (RFC 8617).
    let signed = author.sign(MAIL);
    let msg = AuthenticatedMessage::parse(signed.as_bytes()).expect("parse");
    let arc = check.resolver.verify_arc(check.params(&msg)).await;
    let dkim = check.resolver.verify_dkim(check.params(&msg)).await;
    let ar = AuthenticationResults::new("mx.forwarder.example")
        .with_dkim_results(&dkim, msg.from())
        .with_arc_result(&arc, "192.0.2.10".parse().expect("ip"));
    let set = ArcSealer::from_key(forwarder.key())
        .domain("forwarder.example")
        .selector("arc")
        .headers(["From", "To", "Subject", "DKIM-Signature"])
        .seal(&msg, &ar, &arc)
        .expect("seal");
    let sealed = format!("{}{signed}", set.to_header());
    let out = check
        .check(&envelope("192.0.2.10:1000"), sealed.as_bytes())
        .await;
    assert!(out.headers.contains("arc=pass"), "{}", out.headers);
    assert!(!out.junk);
}

#[tokio::test]
async fn unparsable_message_gets_spf_only() {
    let check = checker();
    publish_sender(check.cache(), "reject");
    let out = check.check(&envelope("192.0.2.10:1000"), b"").await;
    assert!(out.headers.contains("spf=pass"), "{}", out.headers);
    assert!(!out.headers.contains("dkim="));
    assert!(!out.junk);
}

#[test]
fn cache_expires_and_evicts() {
    let c: MemCache<Box<str>, u32> = MemCache::new(2);
    let later = Instant::now() + Duration::from_secs(60);
    c.insert("a".into(), 1, later);
    c.insert("old".into(), 2, Instant::now());
    assert_eq!(c.get("a"), Some(1));
    assert_eq!(c.get("old"), None);
    // Переполнение: истёкшие вытесняются первыми.
    c.insert("b".into(), 3, later);
    assert_eq!((c.get("a"), c.get("b")), (Some(1), Some(3)));
    assert_eq!(c.remove("a"), Some(1));
    assert_eq!(c.get("a"), None);
}
