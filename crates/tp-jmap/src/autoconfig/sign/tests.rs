//! Тесты подписи профиля: разбор SignedData, проверка подписи над `signedAttrs` открытым ключом сертификата
//! подписанта и подписи этого сертификата — следующим в цепочке; профиль внутри — байт в байт исходный.
//!
//! Тестовая PKI (`../testdata`, OpenSSL): `ca.der` — «Test CA» (RSA-512, самоподписанный), `leaf.der` —
//! `CN=mail.example.org`, выпущен «Test CA», серийный номер 4660; [`LEAF_D`] — закрытая экспонента его ключа.
//! RSA-512 — только чтобы арифметика теста на `u32` была быстрой.

use std::time::{Duration, UNIX_EPOCH};

use x509_cert::der::asn1::UintRef;

use super::*;
use crate::JmapConfig;
use crate::autoconfig::mobileconfig_plist;
use crate::autoconfig::tests::servers;
use crate::testenv::{Env, http10, serve};

const LEAF: &[u8] = include_bytes!("../testdata/leaf.der");
const CA: &[u8] = include_bytes!("../testdata/ca.der");
const LEAF_D: &str = "15272a646c4e4b82f553181f6799d6c7aa7c66329811bf1117d1811587fda432\
                      cb8049f39200d862857464b0e15be26c5a1a990028add32305a2d167c79e5801";
/// Префикс `DigestInfo` SHA-256 (RFC 8017 §9.2, примечание 1).
const SHA256_INFO: &[u8] = &[
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

// --- RSA на u32-разрядах (только для теста) ---

fn limbs(be: &[u8], len: usize) -> Vec<u32> {
    let mut v = vec![0u32; len];
    for (i, b) in be.iter().rev().enumerate() {
        v[i / 4] |= u32::from(*b) << (8 * (i % 4));
    }
    v
}

fn bytes(l: &[u32], len: usize) -> Vec<u8> {
    (0..len)
        .rev()
        .map(|i| (l[i / 4] >> (8 * (i % 4))) as u8)
        .collect()
}

/// `a * b mod n`: произведение столбиком и побитовое приведение.
fn mulmod(a: &[u32], b: &[u32], n: &[u32]) -> Vec<u32> {
    let k = n.len();
    let mut p = vec![0u32; 2 * k];
    for i in 0..k {
        let mut carry = 0u64;
        for j in 0..k {
            let t = u64::from(a[i]) * u64::from(b[j]) + u64::from(p[i + j]) + carry;
            p[i + j] = t as u32;
            carry = t >> 32;
        }
        p[i + k] = carry as u32;
    }
    let mut r = vec![0u32; k + 1];
    for bit in (0..64 * k).rev() {
        let mut carry = (p[bit / 32] >> (bit % 32)) & 1;
        for x in &mut r {
            let next = *x >> 31;
            *x = (*x << 1) | carry;
            carry = next;
        }
        let ge = (0..=k)
            .rev()
            .map(|i| (r[i], n.get(i).copied().unwrap_or(0)))
            .find(|(x, y)| x != y)
            .is_none_or(|(x, y)| x > y);
        if ge {
            let mut borrow = 0i64;
            for (i, x) in r.iter_mut().enumerate() {
                let t = i64::from(*x) - i64::from(n.get(i).copied().unwrap_or(0)) - borrow;
                borrow = i64::from(t < 0);
                *x = t.rem_euclid(1 << 32) as u32;
            }
        }
    }
    r.truncate(k);
    r
}

/// `base^exp mod n`, числа — big-endian.
fn modpow(base: &[u8], exp: &[u8], n: &[u8]) -> Vec<u8> {
    let k = n.len().div_ceil(4);
    let (b, m) = (limbs(base, k), limbs(n, k));
    let mut r = limbs(&[1], k);
    for byte in exp {
        for i in (0..8).rev() {
            r = mulmod(&r, &r, &m);
            if byte >> i & 1 == 1 {
                r = mulmod(&r, &b, &m);
            }
        }
    }
    bytes(&r, n.len())
}

/// EMSA-PKCS1-v1_5 с SHA-256 (RFC 8017 §9.2) длиной `k`.
fn emsa(message: &[u8], k: usize) -> Vec<u8> {
    let t = [SHA256_INFO, &Sha256::digest(message)].concat();
    let mut em = vec![0x00, 0x01];
    em.resize(k - t.len() - 1, 0xff);
    em.push(0x00);
    em.extend(t);
    em
}

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

/// Модуль и открытая экспонента RSA из сертификата.
fn rsa_public(cert: &Certificate) -> (Vec<u8>, Vec<u8>) {
    let spki = &cert.tbs_certificate.subject_public_key_info;
    assert_eq!(spki.algorithm.oid, RSA_ENCRYPTION);
    let ints =
        Vec::<UintRef<'_>>::from_der(spki.subject_public_key.raw_bytes()).expect("RSAPublicKey");
    (ints[0].as_bytes().to_vec(), ints[1].as_bytes().to_vec())
}

/// Проверка подписи RSA PKCS#1 v1.5 SHA-256 ключом сертификата.
fn rsa_verify(cert: &Certificate, message: &[u8], signature: &[u8]) -> bool {
    let (n, e) = rsa_public(cert);
    signature.len() == n.len() && modpow(signature, &e, &n) == emsa(message, n.len())
}

/// Ключ `leaf.der` с цепочкой до «Test CA».
struct TestKey {
    chain: Vec<Vec<u8>>,
}

impl TestKey {
    fn new() -> Self {
        Self {
            chain: vec![LEAF.to_vec(), CA.to_vec()],
        }
    }
}

impl SigningKey for TestKey {
    fn chain(&self) -> &[Vec<u8>] {
        &self.chain
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::RsaPkcs1Sha256
    }

    fn sign(&self, message: &[u8]) -> tp_contracts::Result<Vec<u8>> {
        let leaf = Certificate::from_der(LEAF).expect("leaf");
        let (n, _) = rsa_public(&leaf);
        Ok(modpow(&emsa(message, n.len()), &hex(LEAF_D), &n))
    }
}

/// Источник ключа для HTTP-теста: `None` — ключа нет.
struct TestSigner(Option<Arc<dyn SigningKey>>);

#[async_trait]
impl ProfileSigner for TestSigner {
    async fn key(&self) -> Option<Arc<dyn SigningKey>> {
        self.0.clone()
    }
}

fn signed_data(der: &[u8]) -> SignedData {
    let info = ContentInfo::from_der(der).expect("ContentInfo");
    assert_eq!(info.content_type, ID_SIGNED_DATA);
    info.content.decode_as::<SignedData>().expect("SignedData")
}

fn attr_value(signer: &SignerInfo, oid: ObjectIdentifier) -> Any {
    let attrs = signer.signed_attrs.as_ref().expect("signedAttrs");
    let a = attrs.iter().find(|a| a.oid == oid).expect("атрибут");
    assert_eq!(a.values.len(), 1);
    a.values.iter().next().expect("значение").clone()
}

/// Профиль, подписанный ключом `leaf.der` (так же, как снимок `mobileconfig_snapshot`).
fn profile() -> String {
    mobileconfig_plist("User@Example.org", "Иван Петров", "example.org", &servers())
}

/// Подпись проверяется цепочкой: подписант — `leaf.der` по `IssuerAndSerialNumber`, подпись над DER `signedAttrs`
/// верна его ключом, сертификат подписанта подписан «Test CA» из того же набора; `messageDigest` — SHA-256
/// вложенного профиля, а он байт в байт равен неподписанному (снимок не меняется).
#[test]
fn signed_profile_verifies_by_chain() {
    let plist = profile();
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let der = sign(plist.as_bytes(), &TestKey::new(), now).expect("подпись");
    let sd = signed_data(&der);

    assert_eq!(sd.version, CmsVersion::V1);
    assert_eq!(sd.encap_content_info.econtent_type, ID_DATA);
    let content = sd
        .encap_content_info
        .econtent
        .as_ref()
        .expect("вложенное содержимое")
        .decode_as::<OctetString>()
        .expect("OCTET STRING");
    assert_eq!(content.as_bytes(), plist.as_bytes());

    let certs: Vec<Certificate> = sd
        .certificates
        .as_ref()
        .expect("certificates")
        .0
        .iter()
        .map(|c| match c {
            CertificateChoices::Certificate(c) => c.clone(),
            CertificateChoices::Other(_) => panic!("не X.509"),
        })
        .collect();
    assert_eq!(certs.len(), 2);

    assert_eq!(sd.signer_infos.0.len(), 1);
    let signer = sd.signer_infos.0.iter().next().expect("подписант");
    let SignerIdentifier::IssuerAndSerialNumber(sid) = &signer.sid else {
        panic!("sid — не IssuerAndSerialNumber");
    };
    let leaf = certs
        .iter()
        .find(|c| {
            c.tbs_certificate.issuer == sid.issuer
                && c.tbs_certificate.serial_number == sid.serial_number
        })
        .expect("сертификат подписанта в наборе");
    assert_eq!(leaf.to_der().expect("DER"), LEAF);
    assert_eq!(signer.digest_alg.oid, SHA256);
    assert!(sd.digest_algorithms.iter().any(|a| a.oid == SHA256));
    assert_eq!(signer.signature_algorithm.oid, RSA_ENCRYPTION);

    assert_eq!(
        attr_value(signer, ID_CONTENT_TYPE)
            .decode_as::<ObjectIdentifier>()
            .expect("OID"),
        ID_DATA
    );
    let digest = attr_value(signer, ID_MESSAGE_DIGEST)
        .decode_as::<OctetString>()
        .expect("OCTET STRING");
    assert_eq!(
        digest.as_bytes(),
        Sha256::digest(plist.as_bytes()).as_slice()
    );
    let time =
        Time::from_der(&attr_value(signer, ID_SIGNING_TIME).to_der().expect("DER")).expect("Time");
    assert!(matches!(time, Time::UtcTime(_)));
    assert_eq!(UNIX_EPOCH + time.to_unix_duration(), now);

    let attrs = signer
        .signed_attrs
        .as_ref()
        .expect("signedAttrs")
        .to_der()
        .expect("DER");
    let signature = signer.signature.as_bytes();
    assert!(rsa_verify(leaf, &attrs, signature), "подпись signedAttrs");

    let issuer = certs
        .iter()
        .find(|c| c.tbs_certificate.subject == leaf.tbs_certificate.issuer)
        .expect("издатель в цепочке");
    let tbs = leaf.tbs_certificate.to_der().expect("DER");
    assert!(
        rsa_verify(issuer, &tbs, leaf.signature.raw_bytes()),
        "подпись сертификата подписанта издателем"
    );

    // Подмена: другой профиль не совпадает с messageDigest, испорченная подпись не проходит.
    assert_ne!(
        digest.as_bytes(),
        Sha256::digest(profile().replace("Иван", "Пётр")).as_slice()
    );
    let mut bad = signature.to_vec();
    bad[10] ^= 1;
    assert!(!rsa_verify(leaf, &attrs, &bad));
}

/// Ключ ECDSA с постоянной «подписью»: алгоритмы и хэш выбираются по ключу.
struct EcKey(SignatureAlgorithm, Vec<Vec<u8>>);

impl SigningKey for EcKey {
    fn chain(&self) -> &[Vec<u8>] {
        &self.1
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        self.0
    }

    fn sign(&self, _: &[u8]) -> tp_contracts::Result<Vec<u8>> {
        Ok(vec![0x30, 0x00])
    }
}

/// ECDSA: `ecdsa-with-SHA256` и `ecdsa-with-SHA384` без параметров, `messageDigest` — тем же хэшем.
#[test]
fn ecdsa_algorithms() {
    for (alg, sig, digest, len) in [
        (
            SignatureAlgorithm::EcdsaP256Sha256,
            ECDSA_SHA256,
            SHA256,
            32,
        ),
        (
            SignatureAlgorithm::EcdsaP384Sha384,
            ECDSA_SHA384,
            SHA384,
            48,
        ),
    ] {
        let der = sign(
            b"plist",
            &EcKey(alg, vec![LEAF.to_vec()]),
            SystemTime::now(),
        )
        .expect("подпись");
        let sd = signed_data(&der);
        let signer = sd.signer_infos.0.iter().next().expect("подписант");
        assert_eq!(signer.signature_algorithm.oid, sig);
        assert!(signer.signature_algorithm.parameters.is_none());
        assert_eq!(signer.digest_alg.oid, digest);
        let md = attr_value(signer, ID_MESSAGE_DIGEST)
            .decode_as::<OctetString>()
            .expect("OCTET STRING");
        assert_eq!(md.as_bytes(), alg.digest(b"plist"));
        assert_eq!(md.as_bytes().len(), len);
        assert_eq!(signer.signature.as_bytes(), [0x30, 0x00]);
    }
}

/// Ключ без цепочки или с ошибкой подписи — ошибка (профиль уйдёт без подписи).
#[test]
fn sign_errors() {
    struct Broken(Vec<Vec<u8>>);
    impl SigningKey for Broken {
        fn chain(&self) -> &[Vec<u8>] {
            &self.0
        }
        fn algorithm(&self) -> SignatureAlgorithm {
            SignatureAlgorithm::RsaPkcs1Sha256
        }
        fn sign(&self, _: &[u8]) -> tp_contracts::Result<Vec<u8>> {
            Err(tp_contracts::Error::Internal("нет ключа".into()))
        }
    }
    let now = SystemTime::now();
    assert!(matches!(
        sign(b"x", &Broken(Vec::new()), now),
        Err(SignError::EmptyChain)
    ));
    assert!(matches!(
        sign(b"x", &Broken(vec![b"not a cert".to_vec()]), now),
        Err(SignError::Der(_))
    ));
    assert!(matches!(
        sign(b"x", &Broken(vec![LEAF.to_vec()]), now),
        Err(SignError::Key(_))
    ));
}

fn get_profile_request() -> Vec<u8> {
    b"GET /mail/profile.mobileconfig HTTP/1.0\r\nHost: mail.example.org\r\nAuthorization: Bearer token-1\r\n\r\n"
        .to_vec()
}

/// HTTP: с ключом — SignedData с профилем внутри; источник без ключа — профиль без подписи.
#[tokio::test]
async fn http_signed_profile() {
    let cfg = JmapConfig {
        public_url: Some("https://mail.example.org".into()),
        ..JmapConfig::default()
    };
    let key: Arc<dyn SigningKey> = Arc::new(TestKey::new());
    let env = Env::with_signer(
        cfg.clone(),
        Vec::new(),
        Some(Arc::new(TestSigner(Some(key)))),
    )
    .await;
    let addr = serve(env.jmap.clone()).await;
    let (code, head, body) = http10(addr, &get_profile_request()).await;
    assert_eq!(code, 200);
    assert!(
        head.to_lowercase()
            .contains("content-type: application/x-apple-aspen-config")
    );
    let sd = signed_data(&body);
    let content = sd
        .encap_content_info
        .econtent
        .expect("вложенное содержимое")
        .decode_as::<OctetString>()
        .expect("OCTET STRING");
    let plist = String::from_utf8(content.as_bytes().to_vec()).expect("utf-8");
    assert!(plist.starts_with("<?xml"));
    assert!(plist.contains("<string>user@example.org</string>"));

    let env = Env::with_signer(cfg, Vec::new(), Some(Arc::new(TestSigner(None)))).await;
    let addr = serve(env.jmap.clone()).await;
    let (code, _, body) = http10(addr, &get_profile_request()).await;
    assert_eq!(code, 200);
    assert!(body.starts_with(b"<?xml"));
}
