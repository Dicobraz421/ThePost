//! Селектор подписи DKIM из политики организации домена (SMTP-40, решение 97).

use std::sync::Arc;

use tp_contracts::meta::{Policy, PolicyStore, PolicyTarget};
use tp_contracts::transport::{AcceptedDomain, DomainKind, TransportStore};
use tp_contracts::{DomainId, TenantId};
use tp_testkit::{MemMetadata, MemPolicies, MemSecretStore, MemTransport, SystemClock};

use super::tests::{MAIL, RSA_PEM, RSA_PUB, cfg, ed25519, publish, secret, verify};
use super::*;
use crate::authcheck::tests as fx;

/// Подпись с политиками: `example.org` — принятый домен организации, политика домена — `selector`.
async fn signer(store: MemSecretStore, selector: Option<&str>) -> DkimSign {
    let transport = MemTransport::new();
    let domain = DomainId::new();
    transport
        .put_domain(
            0,
            &AcceptedDomain {
                id: domain,
                tenant: TenantId::DEFAULT,
                name: "example.org".into(),
                kind: DomainKind::Authoritative,
            },
        )
        .await
        .expect("accepted domain");
    let policies = MemPolicies::new(&MemMetadata::new(Arc::new(SystemClock)));
    policies
        .set_policy(
            TenantId::DEFAULT,
            PolicyTarget::Domain(domain),
            &Policy {
                dkim_selector: selector.map(str::to_owned),
                ..Policy::default()
            },
        )
        .await
        .expect("policy");
    DkimSign::new(Arc::new(store), cfg()).with_policies(
        Arc::new(policies),
        Arc::new(Routes::new(Arc::new(transport))),
    )
}

/// Селектор политики домена подписывает один: подпись `s=ed1` проходит проверку, ключом `rsa1` письмо не
/// подписано.
#[tokio::test]
async fn policy_selector_signs() {
    let store = MemSecretStore::new(Arc::new(SystemClock));
    let (ed_pem, ed_pub) = ed25519();
    store.set(&secret("rsa1"), RSA_PEM);
    store.set(&secret("ed1"), ed_pem);
    let check = fx::checker();
    publish(&check, "rsa1", "rsa", RSA_PUB);
    publish(&check, "ed1", "ed25519", &ed_pub);
    let dkim = signer(store.clone(), Some("ed1")).await;
    let header = dkim
        .sign(MAIL.as_bytes())
        .await
        .expect("sign")
        .expect("domain is signed");
    assert_eq!(header.matches("DKIM-Signature: ").count(), 1, "{header}");
    assert!(
        header.contains("a=ed25519-sha256") && header.contains("s=ed1"),
        "{header}"
    );
    assert!(!header.contains("s=rsa1"), "{header}");
    let ar = verify(&check, &format!("{header}{MAIL}")).await;
    assert!(ar.contains("dkim=pass"), "{ar}");
    // Политика без селектора — все ключи домена.
    let all = signer(store, None).await;
    let header = all
        .sign(MAIL.as_bytes())
        .await
        .expect("sign")
        .expect("signed");
    assert_eq!(header.matches("DKIM-Signature: ").count(), 2, "{header}");
}

/// Селектор политики без ключа в `smtp.dkim` — `Invalid`: письмо не уходит без подписи.
#[tokio::test]
async fn policy_selector_without_key_is_invalid() {
    let store = MemSecretStore::new(Arc::new(SystemClock));
    store.set(&secret("rsa1"), RSA_PEM);
    store.set(&secret("ed1"), ed25519().0);
    let dkim = signer(store, Some("missing")).await;
    assert!(matches!(
        dkim.sign(MAIL.as_bytes()).await,
        Err(Error::Invalid(_))
    ));
}
