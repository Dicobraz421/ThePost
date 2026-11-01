//! Подпись RSA-SHA256 и Ed25519-SHA256 ключами из `MemSecretStore`; проверка подписи — `mail-auth`
//! (проверки входящей почты `AuthCheck` с записью `_domainkey` в кэше DNS); ротация селектора.

use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use mail_auth::common::parse::TxtRecordParser;
use mail_auth::common::verify::DomainKey;
use mail_auth::{ResolverCache, Txt};
use tp_contracts::secrets::SecretRef;
use tp_testkit::{MemSecretStore, SystemClock};

use super::*;
use crate::authcheck::tests as fx;
use crate::config::{DkimDomain, DkimKeyConfig};

const RSA_PEM: &str = include_str!("testdata/rsa.pem");
const RSA_PUB: &str = include_str!("testdata/rsa.pub.b64");

const MAIL: &str =
    "From: Alice <alice@Example.org>\r\nTo: x@example.com\r\nSubject: hello\r\n\r\nbody\r\n";

fn ed25519() -> (String, String) {
    let der = Ed25519Key::generate_pkcs8().expect("generate");
    let public = Ed25519Key::from_pkcs8_der(&der).expect("key").public_key();
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        B64.encode(&der)
    );
    (pem, B64.encode(public))
}

fn secret(path: &str) -> SecretRef {
    format!("thepost/dkim/{path}#key").parse().expect("ref")
}

fn cfg(selector: &str) -> DkimConfig {
    let mut keys = std::collections::BTreeMap::new();
    keys.insert(
        "rsa1".to_owned(),
        DkimKeyConfig {
            algorithm: DkimAlgorithm::RsaSha256,
            key: secret("rsa1"),
        },
    );
    keys.insert(
        "ed1".to_owned(),
        DkimKeyConfig {
            algorithm: DkimAlgorithm::Ed25519Sha256,
            key: secret("ed1"),
        },
    );
    let mut domains = std::collections::BTreeMap::new();
    domains.insert(
        "example.org".to_owned(),
        DkimDomain {
            selector: selector.into(),
            keys,
        },
    );
    DkimConfig {
        domains,
        ..DkimConfig::default()
    }
}

fn publish(check: &crate::AuthCheck, selector: &str, k: &str, public: &str) {
    let record = format!("v=DKIM1; k={k}; p={}", public.trim());
    check.cache().txt.insert(
        format!("{selector}._domainkey.example.org.").into(),
        Txt::from(DomainKey::parse(record.as_bytes())),
        Instant::now() + Duration::from_secs(3600),
    );
}

/// Результат DKIM в `Authentication-Results` проверок входящей почты.
async fn verify(check: &crate::AuthCheck, signed: &str) -> String {
    let out = check
        .check(&fx::envelope("192.0.2.1:1000"), signed.as_bytes())
        .await;
    out.headers
}

#[tokio::test]
async fn rsa_and_ed25519_signatures_verify_and_selector_rotates() {
    let store = MemSecretStore::new(std::sync::Arc::new(SystemClock));
    let (ed_pem, ed_pub) = ed25519();
    store.set(&secret("rsa1"), RSA_PEM);
    store.set(&secret("ed1"), ed_pem);
    let check = fx::checker();
    publish(&check, "rsa1", "rsa", RSA_PUB);
    publish(&check, "ed1", "ed25519", &ed_pub);

    let dkim = DkimSign::new(std::sync::Arc::new(store), cfg("rsa1"));
    let header = dkim
        .sign(MAIL.as_bytes())
        .await
        .expect("sign")
        .expect("domain is signed");
    assert!(header.starts_with("DKIM-Signature: "));
    assert!(header.contains("a=rsa-sha256"), "{header}");
    assert!(header.contains("s=rsa1"));
    assert!(header.contains("d=example.org"));
    let ar = verify(&check, &format!("{header}{MAIL}")).await;
    assert!(ar.contains("dkim=pass"), "{ar}");
    // Изменённое письмо подпись не проходит.
    let forged = format!("{header}{}", MAIL.replace("body", "forged"));
    assert!(!verify(&check, &forged).await.contains("dkim=pass"));

    // Ротация: активный селектор — Ed25519, без перезапуска.
    dkim.update(cfg("ed1"));
    let header = dkim
        .sign(MAIL.as_bytes())
        .await
        .expect("sign")
        .expect("sig");
    assert!(
        header.contains("a=ed25519-sha256") && header.contains("s=ed1"),
        "{header}"
    );
    let ar = verify(&check, &format!("{header}{MAIL}")).await;
    assert!(ar.contains("dkim=pass"), "{ar}");
}

#[tokio::test]
async fn unsigned_domains_and_missing_keys() {
    let store = MemSecretStore::new(std::sync::Arc::new(SystemClock));
    let dkim = DkimSign::new(std::sync::Arc::new(store.clone()), cfg("rsa1"));
    // Чужой домен и письмо без From не подписываются.
    let other = MAIL.replace("alice@Example.org", "bob@example.net");
    assert_eq!(dkim.sign(other.as_bytes()).await.expect("sign"), None);
    assert_eq!(
        dkim.sign(b"Subject: x\r\n\r\nbody\r\n")
            .await
            .expect("sign"),
        None
    );
    // Ключа нет в хранилище — ошибка (письмо не уходит без подписи).
    assert!(dkim.sign(MAIL.as_bytes()).await.is_err());
    // Неверный PEM — Invalid; селектор без ключа — Invalid.
    store.set(&secret("rsa1"), "not a key");
    assert!(matches!(
        dkim.sign(MAIL.as_bytes()).await,
        Err(Error::Invalid(_))
    ));
    dkim.update(cfg("missing"));
    assert!(matches!(
        dkim.sign(MAIL.as_bytes()).await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test]
async fn queue_signs_before_enqueue() {
    use crate::deliver::Outbound;
    use crate::queue::Queue;
    let store = MemSecretStore::new(std::sync::Arc::new(SystemClock));
    store.set(&secret("rsa1"), RSA_PEM);
    let dkim = std::sync::Arc::new(DkimSign::new(std::sync::Arc::new(store), cfg("rsa1")));
    let env = crate::queue::tests::Env::new();
    let q = Queue::new(env.deps.clone()).with_dkim(dkim);
    q.enqueue(
        tp_contracts::TenantId::DEFAULT,
        "alice@example.org",
        &["x@example.com".into()],
        bytes::Bytes::from_static(MAIL.as_bytes()),
    )
    .await
    .expect("enqueue");
    let key = env.queue.blobs()[0];
    let raw = env.blobs.bytes(&key.path()).expect("stored");
    assert!(raw.starts_with(b"DKIM-Signature: "));
    assert!(raw.ends_with(MAIL.as_bytes()));
}
