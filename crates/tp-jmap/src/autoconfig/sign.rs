//! Подпись профиля `.mobileconfig` (решение 2026-11-11, п. 3): CMS SignedData (RFC 5652) с вложенным содержимым —
//! ключом и цепочкой TLS-сертификата сервера. iOS и macOS показывают такой профиль как «Проверено» и имя
//! организации из сертификата; без подписи — «Не проверен».
//!
//! Структура CMS — крейт `cms` (без `builder`), сама подпись — у узла: [`SigningKey`] подписывает байты
//! `SignedAttributes` ключом TLS (RSA PKCS#1 v1.5 или ECDSA, например через провайдер rustls aws-lc-rs).
//! Источник ключа — [`ProfileSigner`] ([`Jmap::with_profile_signer`](crate::Jmap::with_profile_signer)).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::signed_data::{
    CertificateSet, EncapsulatedContentInfo, SignedData, SignerIdentifier, SignerInfo, SignerInfos,
};
use sha2::{Digest, Sha256, Sha384};
use x509_cert::Certificate;
use x509_cert::attr::Attribute;
use x509_cert::der::asn1::{
    GeneralizedTime, Null, ObjectIdentifier, OctetString, SetOfVec, UtcTime,
};
use x509_cert::der::{Any, Decode, Encode};
use x509_cert::spki::AlgorithmIdentifierOwned;
use x509_cert::time::Time;

/// `id-data` (RFC 5652 §4).
pub(crate) const ID_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1");
/// `id-signedData` (RFC 5652 §5.1).
pub(crate) const ID_SIGNED_DATA: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");
/// Атрибут `contentType` (RFC 5652 §11.1).
pub(crate) const ID_CONTENT_TYPE: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.3");
/// Атрибут `messageDigest` (RFC 5652 §11.2).
pub(crate) const ID_MESSAGE_DIGEST: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.4");
/// Атрибут `signingTime` (RFC 5652 §11.3).
pub(crate) const ID_SIGNING_TIME: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.5");
const SHA256: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");
const SHA384: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.2");
/// `rsaEncryption` — алгоритм подписи RSA PKCS#1 v1.5 в CMS (RFC 3370 §3.2), как у OpenSSL.
const RSA_ENCRYPTION: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
const ECDSA_SHA256: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2");
const ECDSA_SHA384: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.3");

/// Алгоритм ключа подписи. Хэш сообщения подписи (`messageDigest`) — тот же, что у подписи.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureAlgorithm {
    /// RSA PKCS#1 v1.5 с SHA-256 (rustls `RSA_PKCS1_SHA256`).
    RsaPkcs1Sha256,
    /// ECDSA P-256 с SHA-256, подпись в DER (rustls `ECDSA_NISTP256_SHA256`).
    EcdsaP256Sha256,
    /// ECDSA P-384 с SHA-384, подпись в DER (rustls `ECDSA_NISTP384_SHA384`).
    EcdsaP384Sha384,
}

impl SignatureAlgorithm {
    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::RsaPkcs1Sha256 | Self::EcdsaP256Sha256 => Sha256::digest(data).to_vec(),
            Self::EcdsaP384Sha384 => Sha384::digest(data).to_vec(),
        }
    }

    fn digest_alg(self) -> AlgorithmIdentifierOwned {
        let oid = match self {
            Self::RsaPkcs1Sha256 | Self::EcdsaP256Sha256 => SHA256,
            Self::EcdsaP384Sha384 => SHA384,
        };
        AlgorithmIdentifierOwned {
            oid,
            parameters: None,
        }
    }

    fn signature_alg(self) -> AlgorithmIdentifierOwned {
        match self {
            // RFC 3370 §3.2: параметры rsaEncryption — NULL.
            Self::RsaPkcs1Sha256 => AlgorithmIdentifierOwned {
                oid: RSA_ENCRYPTION,
                parameters: Some(Any::from(Null)),
            },
            // RFC 5758 §3.2: параметры ECDSA отсутствуют.
            Self::EcdsaP256Sha256 => AlgorithmIdentifierOwned {
                oid: ECDSA_SHA256,
                parameters: None,
            },
            Self::EcdsaP384Sha384 => AlgorithmIdentifierOwned {
                oid: ECDSA_SHA384,
                parameters: None,
            },
        }
    }
}

/// Ключ подписи: снимок ключа и цепочки TLS-сертификата сервера.
pub trait SigningKey: Send + Sync {
    /// Цепочка сертификатов в DER: первый — сертификат ключа, далее промежуточные (корневой можно не включать).
    fn chain(&self) -> &[Vec<u8>];
    /// Алгоритм ключа.
    fn algorithm(&self) -> SignatureAlgorithm;
    /// Подпись `message` (хэширование — часть алгоритма). ECDSA — подпись в DER (`ECDSA-Sig-Value`).
    fn sign(&self, message: &[u8]) -> tp_contracts::Result<Vec<u8>>;
}

/// Источник ключа подписи профиля: текущий ключ секции `tls` (после перечитывания файлов — новый).
#[async_trait]
pub trait ProfileSigner: Send + Sync {
    /// Текущий ключ; `None` — ключа нет или его тип не поддержан: профиль отдаётся без подписи.
    async fn key(&self) -> Option<Arc<dyn SigningKey>>;
}

/// Ошибка сборки подписи.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SignError {
    #[error("пустая цепочка сертификатов")]
    EmptyChain,
    #[error("DER: {0}")]
    Der(#[from] x509_cert::der::Error),
    #[error("ключ: {0}")]
    Key(tp_contracts::Error),
}

/// CMS SignedData (`ContentInfo`, DER) с вложенным `content`: один подписант — сертификат `chain[0]`
/// (`IssuerAndSerialNumber`), подписанные атрибуты `contentType`, `messageDigest`, `signingTime` (`now`),
/// в `certificates` — вся цепочка.
pub(crate) fn sign(
    content: &[u8],
    key: &dyn SigningKey,
    now: SystemTime,
) -> Result<Vec<u8>, SignError> {
    let certs = key
        .chain()
        .iter()
        .map(|der| Certificate::from_der(der))
        .collect::<Result<Vec<_>, _>>()?;
    let leaf = certs.first().ok_or(SignError::EmptyChain)?;
    let alg = key.algorithm();
    let attr = |oid, value: Any| -> Result<Attribute, SignError> {
        Ok(Attribute {
            oid,
            values: SetOfVec::try_from(vec![value])?,
        })
    };
    let signed_attrs = SetOfVec::try_from(vec![
        attr(ID_CONTENT_TYPE, Any::encode_from(&ID_DATA)?)?,
        attr(
            ID_MESSAGE_DIGEST,
            Any::encode_from(&OctetString::new(alg.digest(content))?)?,
        )?,
        attr(ID_SIGNING_TIME, Any::encode_from(&signing_time(now)?)?)?,
    ])?;
    // RFC 5652 §5.4: подписывается DER атрибутов с тегом SET OF, а не неявным [0] из SignerInfo.
    let signature = key.sign(&signed_attrs.to_der()?).map_err(SignError::Key)?;
    let signer = SignerInfo {
        version: CmsVersion::V1,
        sid: SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
            issuer: leaf.tbs_certificate.issuer.clone(),
            serial_number: leaf.tbs_certificate.serial_number.clone(),
        }),
        digest_alg: alg.digest_alg(),
        signed_attrs: Some(signed_attrs),
        signature_algorithm: alg.signature_alg(),
        signature: OctetString::new(signature)?,
        unsigned_attrs: None,
    };
    let signed = SignedData {
        version: CmsVersion::V1,
        digest_algorithms: SetOfVec::try_from(vec![alg.digest_alg()])?,
        encap_content_info: EncapsulatedContentInfo {
            econtent_type: ID_DATA,
            econtent: Some(Any::encode_from(&OctetString::new(content)?)?),
        },
        certificates: Some(CertificateSet::try_from(
            certs
                .into_iter()
                .map(CertificateChoices::Certificate)
                .collect::<Vec<_>>(),
        )?),
        crls: None,
        signer_infos: SignerInfos::try_from(vec![signer])?,
    };
    Ok(ContentInfo {
        content_type: ID_SIGNED_DATA,
        content: Any::encode_from(&signed)?,
    }
    .to_der()?)
}

/// Время подписи (RFC 5652 §11.3): UTCTime по 2049 год, позже — GeneralizedTime.
fn signing_time(now: SystemTime) -> Result<Time, SignError> {
    let unix = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    Ok(match UtcTime::from_unix_duration(unix) {
        Ok(t) => Time::UtcTime(t),
        Err(_) => Time::GeneralTime(GeneralizedTime::from_unix_duration(unix)?),
    })
}

#[cfg(test)]
mod tests;
