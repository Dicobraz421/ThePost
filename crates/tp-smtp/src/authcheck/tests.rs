//! SPF, DKIM, DMARC и ARC на подписанных фикстурах: ключи Ed25519 создаются в тесте, записи DNS — в кэше
//! проверок вместо сети (мок-резолвер: резолвер без серверов, все ответы — из кэша).

use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use mail_auth::arc::ArcSealer;
use mail_auth::crypto::Ed25519Key;
use mail_auth::dkim::{DkimSigner, DomainKey};
use mail_auth::dmarc::DmarcRecord;
use mail_auth::dns::TxtRecordParser;
use mail_auth::hickory_resolver::config::{ResolverConfig, ResolverOpts};
use mail_auth::spf::SpfRecord;
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
            TxtRecord::from(DomainKey::parse(record.as_bytes())),
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

fn txt(cache: &DnsCache, name: &str, value: TxtRecord) {
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
    let spf = TxtRecord::from(SpfRecord::parse(b"v=spf1 ip4:192.0.2.0/24 -all"));
    txt(cache, "example.com.", spf);
    let dmarc = format!("v=DMARC1; p={policy}");
    txt(
        cache,
        "_dmarc.example.com.",
        TxtRecord::from(DmarcRecord::parse(dmarc.as_bytes())),
    );
    // Обход дерева DMARC (DMARCbis) спрашивает и `_dmarc.com.`: записи нет.
    let none = TxtRecord::Error(mail_auth::Error::Dns(
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
        source: None,
        gateway: None,
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
        .with_dkim_results(&dkim, msg.first_from_address())
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

// ---------- Волна smtp-06: шлюз (SMTP-20) и ARC-подпись пересылки (SMTP-23) ----------

#[tokio::test]
async fn spf_by_original_client_of_gateway() {
    let check = checker();
    publish_sender(check.cache(), "reject");
    // Шлюз 198.51.100.7 вне SPF отправителя; исходный клиент из XCLIENT/XFORWARD — в нём.
    let mut env = envelope("192.0.2.20:1000");
    env.gateway = Some("198.51.100.7:25".parse().expect("addr"));
    let out = check.check(&env, MAIL.as_bytes()).await;
    assert!(out.headers.contains("spf=pass"), "{}", out.headers);
    assert!(out.headers.contains("192.0.2.20"), "{}", out.headers);
    let out = check
        .check(&envelope("198.51.100.7:25"), MAIL.as_bytes())
        .await;
    assert!(out.headers.contains("spf=fail"), "{}", out.headers);
}

#[tokio::test]
async fn gateway_results_by_authserv_id() {
    let check = checker();
    let raw = format!(
        "Authentication-Results: other.example; dmarc=pass\r\n\
         Authentication-Results: GW.example.org 1;\r\n\tspf=fail smtp.mailfrom=example.com;\r\n\t\
         dmarc=fail header.from=example.com policy.dmarc=reject\r\n{MAIL}"
    );
    let out = check
        .gateway(raw.as_bytes(), "gw.example.org")
        .await
        .expect("results");
    assert!(out.junk);
    assert!(out.headers.is_empty());
    let pass = format!(
        "Authentication-Results: gw.example.org; dmarc=pass header.from=example.com\r\n{MAIL}"
    );
    let out = check
        .gateway(pass.as_bytes(), "gw.example.org")
        .await
        .expect("results");
    assert!(!out.junk);
    assert!(out.dmarc_pass);
    // Результатов шлюза нет — обычные проверки.
    assert!(
        check
            .gateway(raw.as_bytes(), "mx.other.example")
            .await
            .is_none()
    );
    assert!(
        check
            .gateway(MAIL.as_bytes(), "gw.example.org")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn gateway_results_from_arc() {
    let check = checker();
    let gw = Signer::new("gw.example.org", "arc");
    gw.publish(check.cache());
    let msg = AuthenticatedMessage::parse(MAIL.as_bytes()).expect("parse");
    let arc = check.resolver.verify_arc(check.params(&msg)).await;
    let ar = AuthenticationResults::new("gw.example.org")
        .with_arc_result(&arc, "192.0.2.10".parse().expect("ip"));
    let set = ArcSealer::from_key(gw.key())
        .domain("gw.example.org")
        .selector("arc")
        .headers(["From", "To", "Subject"])
        .seal(&msg, &ar, &arc)
        .expect("seal");
    let sealed = format!("{}{MAIL}", set.to_header());
    assert!(
        check
            .gateway(sealed.as_bytes(), "gw.example.org")
            .await
            .is_some()
    );
    // Звено поставил другой authserv-id — не доверяем.
    assert!(
        check
            .gateway(sealed.as_bytes(), "other.example")
            .await
            .is_none()
    );
}

// ---------- Волна smtp-28: доверие к шлюзу (SMTP-47) ----------

/// DMARC пройден по результатам шлюза `results` в `Authentication-Results` письма `mail`.
async fn gateway_dmarc(check: &AuthCheck, results: &str, mail: &str) -> bool {
    let raw = format!("Authentication-Results: gw.example.org; {results}\r\n{mail}");
    check
        .gateway(raw.as_bytes(), "gw.example.org")
        .await
        .expect("results")
        .dmarc_pass
}

#[tokio::test]
async fn gateway_dmarc_pass_needs_header_from_of_message() {
    let check = checker();
    let pass = "spf=pass smtp.mailfrom=example.com;\r\n\tdmarc=pass (p=reject dis=none) header.from=Example.COM.";
    assert!(gateway_dmarc(&check, pass, MAIL).await);
    // Поддельный AR: `dmarc=pass` о другом домене, без `header.from=` или рядом с несовпадающим результатом.
    for forged in [
        "dmarc=pass header.from=attacker.example",
        "dmarc=pass",
        "dmarc=pass header.from=example.com; dmarc=pass header.from=attacker.example",
        "dmarc=fail header.from=example.com policy.dmarc=none",
        "spf=pass smtp.mailfrom=example.com",
    ] {
        assert!(!gateway_dmarc(&check, forged, MAIL).await, "{forged}");
    }
    // Адреса `From` разных доменов (в одном заголовке или в двух): домена автора нет — DMARC не пройден.
    let list = MAIL.replace(
        "From: sender@example.com",
        "From: sender@example.com, x@attacker.example",
    );
    assert!(!gateway_dmarc(&check, pass, &list).await);
    let two = MAIL.replace(
        "From: sender@example.com\r\n",
        "From: sender@example.com\r\nFrom: x@attacker.example\r\n",
    );
    assert!(!gateway_dmarc(&check, pass, &two).await);
}

/// Письмо `MAIL` со звеном ARC: печать ключом `sealer`, результаты — с authserv-id `authserv_id`.
async fn arc_sealed(check: &AuthCheck, sealer: &Signer, authserv_id: &str) -> String {
    let msg = AuthenticatedMessage::parse(MAIL.as_bytes()).expect("parse");
    let arc = check.resolver.verify_arc(check.params(&msg)).await;
    let ar = AuthenticationResults::new(authserv_id)
        .with_arc_result(&arc, "192.0.2.10".parse().expect("ip"));
    let set = ArcSealer::from_key(sealer.key())
        .domain(sealer.domain)
        .selector(sealer.selector)
        .headers(["From", "To", "Subject"])
        .seal(&msg, &ar, &arc)
        .expect("seal");
    format!("{}{MAIL}", set.to_header())
}

#[tokio::test]
async fn gateway_arc_needs_seal_of_gateway_domain() {
    let check = checker();
    // Поддельный ARC: цепочка верна, authserv-id шлюза, но печать — ключом чужого домена.
    let attacker = Signer::new("attacker.example", "arc");
    attacker.publish(check.cache());
    let forged = arc_sealed(&check, &attacker, "gw.example.org").await;
    assert!(
        check
            .gateway(forged.as_bytes(), "gw.example.org")
            .await
            .is_none()
    );
    // Печать родительского домена шлюза — звено шлюза.
    let org = Signer::new("example.org", "arc");
    org.publish(check.cache());
    let sealed = arc_sealed(&check, &org, "gw.example.org").await;
    assert!(
        check
            .gateway(sealed.as_bytes(), "gw.example.org")
            .await
            .is_some()
    );
}

#[test]
fn gateway_domain_of_seal() {
    for (host, domain, want) in [
        ("gw.example.org", "gw.example.org", true),
        ("GW.example.org.", "example.ORG", true),
        ("gw.example.org", "attacker.example", false),
        ("gw.example.org", "le.org", false),
        ("gw.example.org", "xgw.example.org", false),
        ("example.org", "gw.example.org", false),
        // Домен верхнего уровня — не домен шлюза.
        ("gw.example.org", "org", false),
    ] {
        assert_eq!(within(host, domain), want, "{host} / {domain}");
    }
}

#[tokio::test]
async fn forward_arc_seal_verifies() {
    let check = checker();
    let author = Signer::new("example.com", "s1");
    author.publish(check.cache());
    let node = Signer::new("example.org", "arc2026");
    node.publish(check.cache());
    let sealer = ArcSealer::from_key(mail_auth::crypto::DkimKey::Ed25519(node.key()))
        .domain("example.org")
        .selector("arc2026")
        .headers(["From", "To", "Subject"]);
    let signed = author.sign(MAIL);
    let set = check
        .seal(
            &sealer,
            signed.as_bytes(),
            Some("192.0.2.10".parse().expect("ip")),
        )
        .await
        .expect("sealed");
    assert!(set.contains("ARC-Seal:"), "{set}");
    let forwarded = format!("{set}{signed}");
    // Получатель пересылки проверяет цепочку ARC (mail-auth).
    let msg = AuthenticatedMessage::parse(forwarded.as_bytes()).expect("parse");
    let arc = check.resolver.verify_arc(check.params(&msg)).await;
    assert!(
        matches!(arc.result(), mail_auth::DkimResult::Pass),
        "{:?}",
        arc.result()
    );
    assert_eq!(arc.chain().len(), 1);
}
