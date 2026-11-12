//! Подпись RSA-SHA256 и Ed25519-SHA256 ключами из `MemSecretStore`; проверка подписи — `mail-auth`
//! (проверки входящей почты `AuthCheck` с записью `_domainkey` в кэше DNS); без политики — все ключи домена.

use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use mail_auth::dkim::DomainKey;
use mail_auth::dns::TxtRecordParser;
use mail_auth::{ResolverCache, TxtRecord};
use tp_contracts::secrets::SecretRef;
use tp_testkit::{MemSecretStore, SystemClock};

use super::*;
use crate::authcheck::tests as fx;
use crate::config::{DkimDomain, DkimKeyConfig};

pub(super) const RSA_PEM: &str = include_str!("testdata/rsa.pem");
pub(super) const RSA_PUB: &str = include_str!("testdata/rsa.pub.b64");

pub(super) const MAIL: &str =
    "From: Alice <alice@Example.org>\r\nTo: x@example.com\r\nSubject: hello\r\n\r\nbody\r\n";

pub(super) fn ed25519() -> (String, String) {
    let der = Ed25519Key::generate_pkcs8().expect("generate");
    let public = Ed25519Key::from_pkcs8_der(&der).expect("key").public_key();
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        B64.encode(&der)
    );
    (pem, B64.encode(public))
}

pub(super) fn secret(path: &str) -> SecretRef {
    format!("thepost/dkim/{path}#key").parse().expect("ref")
}

pub(super) fn cfg() -> DkimConfig {
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
    domains.insert("example.org".to_owned(), DkimDomain { keys });
    DkimConfig {
        domains,
        ..DkimConfig::default()
    }
}

pub(super) fn publish(check: &crate::AuthCheck, selector: &str, k: &str, public: &str) {
    let record = format!("v=DKIM1; k={k}; p={}", public.trim());
    check.cache().txt.insert(
        format!("{selector}._domainkey.example.org.").into(),
        TxtRecord::from(DomainKey::parse(record.as_bytes())),
        Instant::now() + Duration::from_secs(3600),
    );
}

/// Результат DKIM в `Authentication-Results` проверок входящей почты.
pub(super) async fn verify(check: &crate::AuthCheck, signed: &str) -> String {
    let out = check
        .check(&fx::envelope("192.0.2.1:1000"), signed.as_bytes())
        .await;
    out.headers
}

/// Без политики письмо подписывается всеми ключами домена: подписи RSA и Ed25519 проходят проверку.
#[tokio::test]
async fn rsa_and_ed25519_signatures_verify() {
    let store = MemSecretStore::new(std::sync::Arc::new(SystemClock));
    let (ed_pem, ed_pub) = ed25519();
    store.set(&secret("rsa1"), RSA_PEM);
    store.set(&secret("ed1"), ed_pem);
    let check = fx::checker();
    publish(&check, "rsa1", "rsa", RSA_PUB);
    publish(&check, "ed1", "ed25519", &ed_pub);

    let dkim = DkimSign::new(std::sync::Arc::new(store), cfg());
    let header = dkim
        .sign(MAIL.as_bytes())
        .await
        .expect("sign")
        .expect("domain is signed");
    assert!(header.starts_with("DKIM-Signature: "));
    assert_eq!(header.matches("DKIM-Signature: ").count(), 2, "{header}");
    assert!(
        header.contains("a=rsa-sha256") && header.contains("s=rsa1"),
        "{header}"
    );
    assert!(
        header.contains("a=ed25519-sha256") && header.contains("s=ed1"),
        "{header}"
    );
    assert!(header.contains("d=example.org"));
    let ar = verify(&check, &format!("{header}{MAIL}")).await;
    assert_eq!(ar.matches("dkim=pass").count(), 2, "{ar}");
    // Изменённое письмо подпись не проходит.
    let forged = format!("{header}{}", MAIL.replace("body", "forged"));
    assert!(!verify(&check, &forged).await.contains("dkim=pass"));
}

#[tokio::test]
async fn unsigned_domains_and_missing_keys() {
    let store = MemSecretStore::new(std::sync::Arc::new(SystemClock));
    store.set(&secret("ed1"), ed25519().0);
    let dkim = DkimSign::new(std::sync::Arc::new(store.clone()), cfg());
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
    // Неверный PEM — Invalid.
    store.set(&secret("rsa1"), "not a key");
    assert!(matches!(
        dkim.sign(MAIL.as_bytes()).await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test]
async fn queue_signs_before_enqueue() {
    use crate::queue::Queue;
    use tp_contracts::queue::Outbound;
    let store = MemSecretStore::new(std::sync::Arc::new(SystemClock));
    store.set(&secret("rsa1"), RSA_PEM);
    store.set(&secret("ed1"), ed25519().0);
    let dkim = std::sync::Arc::new(DkimSign::new(std::sync::Arc::new(store), cfg()));
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
