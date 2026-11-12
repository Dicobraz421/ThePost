//! Подпись профиля `.mobileconfig` ключом TLS узла (APP-23, решение 2026-11-11, п. 20): цепочка и ключ — текущие
//! у слушателей ([`TlsCerts::certified_key`], PLT-23), после перечитывания сертификата (ACME, событие ОС, SIGHUP)
//! профиль подписывается новым. Закрытый ключ не покидает rustls: подпись — через его `SigningKey`.
//!
//! RSA — PKCS#1 v1.5 с SHA-256, ECDSA P-256 и P-384 — со своим хэшем; ключ другого типа (Ed25519 — нет в
//! [`SignatureAlgorithm`] tp-jmap) и узел без TLS — профиль без подписи.

use std::sync::Arc;

use async_trait::async_trait;
use tp_contracts::{Error, Result};
use tp_jmap::{ProfileSigner, SignatureAlgorithm, SigningKey};
use tp_platform::TlsCerts;
use tp_platform::rustls::SignatureScheme;
use tp_platform::rustls::sign::{CertifiedKey, SigningKey as TlsKey};

/// Схемы rustls поддержанных алгоритмов подписи профиля; ключ подходит не более чем к одной.
const SCHEMES: [(SignatureScheme, SignatureAlgorithm); 3] = [
    (
        SignatureScheme::RSA_PKCS1_SHA256,
        SignatureAlgorithm::RsaPkcs1Sha256,
    ),
    (
        SignatureScheme::ECDSA_NISTP256_SHA256,
        SignatureAlgorithm::EcdsaP256Sha256,
    ),
    (
        SignatureScheme::ECDSA_NISTP384_SHA384,
        SignatureAlgorithm::EcdsaP384Sha384,
    ),
];

/// Источник ключа подписи профиля для JMAP: текущие цепочка и ключ TLS узла на каждый запрос профиля.
#[derive(Debug)]
pub struct TlsProfileSigner {
    certs: Option<Arc<TlsCerts>>,
}

impl TlsProfileSigner {
    /// Ключ из `certs`; `None` — узел без TLS, профиль без подписи.
    pub fn new(certs: Option<Arc<TlsCerts>>) -> Self {
        Self { certs }
    }
}

/// Подпись профиля для `Jmap::with_profile_signer`, как подключает её узел (`Services::jmap`).
pub fn profile_signer(certs: Option<Arc<TlsCerts>>) -> Option<Arc<dyn ProfileSigner>> {
    Some(Arc::new(TlsProfileSigner::new(certs)))
}

#[async_trait]
impl ProfileSigner for TlsProfileSigner {
    async fn key(&self) -> Option<Arc<dyn SigningKey>> {
        let certified = self.certs.as_ref()?.certified_key();
        let key = TlsSigningKey::new(&certified)?;
        Some(Arc::new(key))
    }
}

/// Снимок ключа TLS для одной подписи профиля.
struct TlsSigningKey {
    chain: Vec<Vec<u8>>,
    scheme: SignatureScheme,
    algorithm: SignatureAlgorithm,
    key: Arc<dyn TlsKey>,
}

impl TlsSigningKey {
    /// `None` — тип ключа не поддержан (Ed25519 и прочие).
    fn new(certified: &CertifiedKey) -> Option<Self> {
        let (scheme, algorithm) = SCHEMES
            .into_iter()
            .find(|(scheme, _)| certified.key.choose_scheme(&[*scheme]).is_some())?;
        Some(Self {
            chain: certified.cert.iter().map(|c| c.as_ref().to_vec()).collect(),
            scheme,
            algorithm,
            key: certified.key.clone(),
        })
    }
}

impl SigningKey for TlsSigningKey {
    fn chain(&self) -> &[Vec<u8>] {
        &self.chain
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        self.algorithm
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        let signer = self
            .key
            .choose_scheme(&[self.scheme])
            .ok_or_else(|| Error::Internal(format!("tls key: no {:?} signer", self.scheme)))?;
        signer
            .sign(message)
            .map_err(|e| Error::Internal(format!("tls key: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::net::SocketAddr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tp_contracts::auth::Principal;
    use tp_contracts::ext::DecisionHooks;
    use tp_platform::TlsConfig;
    use tp_testkit::{
        Clock, FakeAuthenticator, FakeSearch, MemAcl, MemBlobIndex, MemBlobStore, MemDeliveryLog,
        MemEventBus, MemMailboxAudit, MemMetadata, MemPolicies, MemQuota, MemSecretStore,
        MemSubmissions, MemTrash, SystemClock,
    };
    use x509_parser::asn1_rs::{Any, FromDer};

    use super::*;

    /// Пары сертификат и ключ (самоподписанные, `testdata`).
    const P256: (&str, &str) = (
        include_str!("testdata/cert.pem"),
        include_str!("testdata/key.pem"),
    );
    const P384: (&str, &str) = (
        include_str!("testdata/cert-p384.pem"),
        include_str!("testdata/key-p384.pem"),
    );
    const RSA: (&str, &str) = (
        include_str!("testdata/cert-rsa.pem"),
        include_str!("testdata/key-rsa.pem"),
    );
    const ED25519: (&str, &str) = (
        include_str!("testdata/cert-ed25519.pem"),
        include_str!("testdata/key-ed25519.pem"),
    );
    /// DER OID `id-signedData` (1.2.840.113549.1.7.2).
    const ID_SIGNED_DATA: &[u8] = &[
        0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02,
    ];

    /// Цепочка — в файл секции, ключ — новой версией секрета `tls.key` в `store`.
    fn write_pair(cfg: &TlsConfig, store: &MemSecretStore, (cert, key): (&str, &str)) {
        std::fs::write(&cfg.cert_file, cert).expect("write cert");
        store.set(&cfg.key, key);
    }

    fn der(pem: &str) -> Vec<u8> {
        let (_, pem) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).expect("pem");
        pem.contents
    }

    /// Сервер JMAP на хранилищах testkit с подписью профиля, как подключает её узел.
    async fn jmap(certs: Option<Arc<TlsCerts>>) -> Arc<tp_jmap::Jmap> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let index = MemBlobIndex::new(clock.clone());
        let blobs = MemBlobStore::new(clock.clone());
        let meta = MemMetadata::new(clock).with_blob_index(index.clone());
        let bus = MemEventBus::default();
        let policies = MemPolicies::new(&meta);
        let trash = MemTrash::new(&meta).with_policies(policies.clone());
        let store = tp_mailstore::Store::new(tp_mailstore::Deps {
            blobs: Arc::new(blobs.clone()),
            index: Arc::new(index.clone()),
            messages: Arc::new(meta.clone()),
            changes: Arc::new(meta.clone()),
            trash: Arc::new(trash.clone()),
            quota: Arc::new(MemQuota::new(&meta)),
            bus: Arc::new(bus.clone()),
            hooks: DecisionHooks::default(),
        });
        let scope = meta.account("user@example.org").await.expect("account");
        let auth = FakeAuthenticator::default();
        auth.add_token(
            "token-1",
            Principal {
                tenant: scope.tenant,
                account: scope.account,
                login: "user@example.org".into(),
                groups: BTreeSet::new(),
                scopes: None,
            },
        );
        let acl = Arc::new(MemAcl::new(&meta));
        let accounts = Arc::new(meta.clone());
        let deps = tp_jmap::Deps {
            auth: Arc::new(auth),
            mailboxes: Arc::new(meta.clone()),
            messages: Arc::new(meta.clone()),
            changes: Arc::new(meta.clone()),
            store: Arc::new(store),
            trash: Arc::new(trash),
            policies: Arc::new(policies),
            search: Arc::new(FakeSearch::default()),
            blobs: Arc::new(blobs),
            blob_index: Arc::new(index),
            bus: Arc::new(bus),
            accounts: accounts.clone(),
            authz: Arc::new(tp_auth::AclAuthorizer::new(acl, accounts)),
            outbound: None,
            submissions: Arc::new(MemSubmissions::new()),
            delivery_log: Arc::new(MemDeliveryLog::new()),
            audit: Arc::new(MemMailboxAudit::new()),
            hooks: DecisionHooks::default(),
            capabilities: Vec::new(),
        };
        let cfg = tp_jmap::JmapConfig {
            public_url: Some("https://mail.example.org".into()),
            ..tp_jmap::JmapConfig::default()
        };
        Arc::new(
            tp_jmap::Jmap::new(deps, cfg)
                .expect("jmap")
                .with_profile_signer(profile_signer(certs)),
        )
    }

    async fn serve(jmap: Arc<tp_jmap::Jmap>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let router = jmap.router();
        tokio::spawn(async move { axum::serve(listener, router).await });
        addr
    }

    /// Тело ответа на запрос профиля (HTTP/1.0: без chunked, до закрытия соединения).
    async fn profile(addr: SocketAddr) -> Vec<u8> {
        let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
        s.write_all(
            b"GET /mail/profile.mobileconfig HTTP/1.0\r\nHost: mail.example.org\r\n\
              Authorization: Bearer token-1\r\n\r\n",
        )
        .await
        .expect("write");
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.expect("read");
        let split = out
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("headers")
            + 4;
        assert!(out.starts_with(b"HTTP/1.0 200"), "{:?}", &out[..split]);
        out[split..].to_vec()
    }

    /// Объекты DER подряд из `input` и полная кодировка каждого.
    fn items(mut input: &[u8]) -> Vec<(Any<'_>, &[u8])> {
        let mut out = Vec::new();
        while !input.is_empty() {
            let (rest, any) = Any::from_der(input).expect("DER");
            out.push((any, &input[..input.len() - rest.len()]));
            input = rest;
        }
        out
    }

    /// Разобранный CMS SignedData профиля.
    struct Signed {
        content: Vec<u8>,
        certs: Vec<Vec<u8>>,
        /// DER `signedAttrs` с тегом SET OF — то, что подписано (RFC 5652 §5.4).
        attrs: Vec<u8>,
        signature: Vec<u8>,
    }

    fn parse_signed(der: &[u8]) -> Signed {
        let info = items(der);
        assert_eq!(info.len(), 1, "один ContentInfo");
        let info = items(info[0].0.data);
        assert_eq!(info[0].1, ID_SIGNED_DATA);
        // SignedData: version, digestAlgorithms, encapContentInfo, [0] certificates, signerInfos.
        let sd = items(items(info[1].0.data)[0].0.data);
        assert_eq!(sd.len(), 5, "SignedData без crls");
        let encap = items(sd[2].0.data);
        let content = items(encap[1].0.data)[0].0.data.to_vec();
        let certs = items(sd[3].0.data)
            .into_iter()
            .map(|(_, raw)| raw.to_vec())
            .collect();
        let signers = items(sd[4].0.data);
        assert_eq!(signers.len(), 1, "один подписант");
        // SignerInfo: version, sid, digestAlgorithm, [0] signedAttrs, signatureAlgorithm, signature.
        let si = items(signers[0].0.data);
        let mut attrs = si[3].1.to_vec();
        attrs[0] = 0x31;
        Signed {
            content,
            certs,
            attrs,
            signature: si[5].0.data.to_vec(),
        }
    }

    /// Подпись `sig` над `msg` верна открытым ключом сертификата `cert` по схеме `scheme`.
    fn verifies(scheme: SignatureScheme, cert: &[u8], msg: &[u8], sig: &[u8]) -> bool {
        let (_, cert) = x509_parser::parse_x509_certificate(cert).expect("cert");
        let key: &[u8] = &cert.public_key().subject_public_key.data;
        let algs = tp_platform::rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms;
        let (_, verifiers) = algs
            .mapping
            .iter()
            .find(|(s, _)| *s == scheme)
            .expect("verifier");
        verifiers[0].verify_signature(key, msg, sig).is_ok()
    }

    /// Профиль, отданный узлом, подписан текущим сертификатом и проверяется его цепочкой; после перечитывания
    /// сертификата — новым (P-256 → P-384 → RSA). Ed25519 и узел без TLS — профиль без подписи.
    #[tokio::test]
    async fn node_profile_signed_by_current_certificate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = TlsConfig {
            cert_file: dir.path().join("cert.pem"),
            ..TlsConfig::default()
        };
        let store = Arc::new(MemSecretStore::default());
        write_pair(&cfg, &store, P256);
        let certs = Arc::new(TlsCerts::load(&cfg, store.clone()).await.expect("load"));
        let addr = serve(jmap(Some(certs.clone())).await).await;

        let pairs = [
            (P256, SignatureScheme::ECDSA_NISTP256_SHA256),
            (P384, SignatureScheme::ECDSA_NISTP384_SHA384),
            (RSA, SignatureScheme::RSA_PKCS1_SHA256),
        ];
        let mut previous: Option<Vec<u8>> = None;
        for (pair, scheme) in pairs {
            write_pair(&cfg, &store, pair);
            certs.reload().await.expect("reload");
            let signed = parse_signed(&profile(addr).await);
            assert_eq!(
                signed.certs,
                vec![der(pair.0)],
                "{scheme:?}: цепочка текущая"
            );
            let leaf = &signed.certs[0];
            assert!(
                verifies(scheme, leaf, &signed.attrs, &signed.signature),
                "{scheme:?}: подпись проверяется сертификатом"
            );
            let mut tampered = signed.attrs.clone();
            let last = tampered.len() - 1;
            tampered[last] ^= 1;
            assert!(!verifies(scheme, leaf, &tampered, &signed.signature));
            if let Some(old) = &previous {
                assert_ne!(old, leaf, "после перечитывания — новый сертификат");
            }
            previous = Some(leaf.clone());
            let plist = String::from_utf8(signed.content).expect("utf-8");
            assert!(plist.starts_with("<?xml"), "{plist}");
            assert!(plist.contains("<string>user@example.org</string>"));
        }

        write_pair(&cfg, &store, ED25519);
        certs.reload().await.expect("reload ed25519");
        assert!(
            profile(addr).await.starts_with(b"<?xml"),
            "Ed25519 — без подписи"
        );

        let plain = serve(jmap(None).await).await;
        assert!(
            profile(plain).await.starts_with(b"<?xml"),
            "без TLS — без подписи"
        );
    }
}
