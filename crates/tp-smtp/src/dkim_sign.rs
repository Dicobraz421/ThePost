//! DKIM-подпись исходящей почты (RFC 6376, RFC 8463) на `mail-auth`: RSA-SHA256 и Ed25519-SHA256.
//!
//! Подписываются письма, домен `From` которых есть в `smtp.dkim.domains`; ключ — активный селектор домена
//! (`selector`), закрытый ключ (PEM: PKCS#8 или PKCS#1 для RSA, PKCS#8 для Ed25519) — из хранилища секретов
//! по ссылке `key` (решение 2026-10-30: секреты — только во внешнем хранилище). Разобранные ключи кэшируются
//! на `key_refresh`.
//!
//! Ротация селекторов: новый ключ публикуется в DNS (`<селектор>._domainkey.<домен>`) и добавляется в `keys`,
//! затем `selector` переключается на него (без перезапуска, [`DkimSign::update`]); старый селектор остаётся
//! в DNS, пока письма с его подписью могут проверяться. Подпись ставит [`crate::queue::Queue`] при постановке
//! в очередь — до первой попытки, один раз на письмо.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use mail_auth::common::crypto::{DkimKey as SigningKeyPair, Ed25519Key, RsaKey, Sha256};
use mail_auth::common::headers::HeaderWriter;
use mail_auth::dkim::{DkimSigner, Done};
use mail_parser::MessageParser;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tp_contracts::secrets::SecretStore;
use tp_contracts::{Error, Result};

use crate::config::{DkimAlgorithm, DkimConfig};

type Signer = DkimSigner<SigningKeyPair, Done>;

/// Разобранные ключи: (домен, селектор) → (момент загрузки, подпись).
type KeyCache = HashMap<(String, String), (Instant, Arc<Signer>)>;

/// Подпись DKIM с ключами из хранилища секретов.
pub struct DkimSign {
    secrets: Arc<dyn SecretStore>,
    cfg: RwLock<Arc<DkimConfig>>,
    cache: Mutex<KeyCache>,
}

impl fmt::Debug for DkimSign {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DkimSign")
            .field("domains", &read(&self.cfg).domains.len())
            .finish_non_exhaustive()
    }
}

impl DkimSign {
    /// Подпись по секции `cfg` с ключами из `secrets`.
    pub fn new(secrets: Arc<dyn SecretStore>, cfg: DkimConfig) -> Self {
        Self {
            secrets,
            cfg: RwLock::new(Arc::new(cfg)),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Новая версия секции (hot reload): ключи сменённых селекторов перечитываются.
    pub fn update(&self, cfg: DkimConfig) {
        *self
            .cfg
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(cfg);
        lock(&self.cache).clear();
    }

    /// Заголовок `DKIM-Signature` (с `\r\n`) для письма `raw`; домен `From` без ключа — `None`.
    pub async fn sign(&self, raw: &[u8]) -> Result<Option<String>> {
        let Some(domain) = from_domain(raw) else {
            return Ok(None);
        };
        let cfg = read(&self.cfg).clone();
        let Some(d) = cfg.domains.get(&domain) else {
            return Ok(None);
        };
        let signer = self.signer(&cfg, &domain, &d.selector).await?;
        let sig = signer
            .sign(raw)
            .map_err(|e| Error::Internal(format!("dkim sign {domain}: {e}")))?;
        Ok(Some(sig.to_header()))
    }

    async fn signer(&self, cfg: &DkimConfig, domain: &str, selector: &str) -> Result<Arc<Signer>> {
        let id = (domain.to_owned(), selector.to_owned());
        if let Some((at, s)) = lock(&self.cache).get(&id)
            && at.elapsed() < cfg.key_refresh
        {
            return Ok(s.clone());
        }
        let key = cfg
            .domains
            .get(domain)
            .and_then(|d| d.keys.get(selector))
            .ok_or_else(|| {
                Error::Invalid(format!("dkim: no key for selector {selector} of {domain}"))
            })?;
        let secret = self.secrets.get(&key.key).await?;
        let pair = parse_key(key.algorithm, secret.expose())
            .map_err(|e| Error::Invalid(format!("dkim key {selector}._domainkey.{domain}: {e}")))?;
        let signer = Arc::new(
            DkimSigner::from_key(pair)
                .domain(domain)
                .selector(selector)
                .headers(cfg.headers.iter().cloned()),
        );
        lock(&self.cache).insert(id, (Instant::now(), signer.clone()));
        Ok(signer)
    }
}

/// Закрытый ключ из PEM.
fn parse_key(alg: DkimAlgorithm, pem: &str) -> std::result::Result<SigningKeyPair, String> {
    let der = PrivateKeyDer::from_pem_slice(pem.as_bytes()).map_err(|e| e.to_string())?;
    Ok(match alg {
        DkimAlgorithm::RsaSha256 => {
            SigningKeyPair::Rsa(RsaKey::<Sha256>::from_key_der(der).map_err(|e| e.to_string())?)
        }
        DkimAlgorithm::Ed25519Sha256 => {
            let PrivateKeyDer::Pkcs8(der) = der else {
                return Err("Ed25519 key must be PKCS#8".into());
            };
            let der: PrivatePkcs8KeyDer<'_> = der;
            SigningKeyPair::Ed25519(
                Ed25519Key::from_pkcs8_maybe_unchecked_der(der.secret_pkcs8_der())
                    .map_err(|e| e.to_string())?,
            )
        }
    })
}

/// Домен первого адреса `From` (нижний регистр).
fn from_domain(raw: &[u8]) -> Option<String> {
    let msg = MessageParser::default().parse_headers(raw)?;
    let addr = msg.from()?.first()?.address()?;
    addr.rsplit_once('@').map(|(_, d)| d.to_lowercase())
}

fn read<T: Clone>(l: &RwLock<T>) -> T {
    l.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
