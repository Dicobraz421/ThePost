//! Политики TLS при отправке на MX: MTA-STS (RFC 8461) и DANE (RFC 7672).
//!
//! - MTA-STS: запись TXT `_mta-sts.<домен>` (разбор — `mail-auth`), политика
//!   `https://mta-sts.<домен>/.well-known/mta-sts.txt` (`reqwest`: только HTTPS с проверкой сертификата,
//!   без перенаправлений, `queue.policy.fetch_timeout`, не больше `queue.policy.max_policy_size`). Политика
//!   кэшируется в памяти узла на `max_age` и перечитывается при смене `id` в TXT; пока она не истекла, сбой DNS
//!   или HTTPS её не отменяет (RFC 8461, 5.1). Режим `enforce`: узлы MX — только из списка `mx` политики,
//!   STARTTLS с проверкой сертификата по системным корням и имени MX; иначе узел пропускается.
//!   `testing` — несоответствия только в журнал.
//! - DANE: записи TLSA `_<порт>._tcp.<узел MX>`, подтверждённые DNSSEC (резолвер hickory с проверкой подписей).
//!   Есть такие записи — STARTTLS обязателен (без повтора открытым текстом), сертификат сверяется с
//!   записями DANE-EE (3) и DANE-TA (2): DANE-EE — сертификат сервера или его ключ, без проверки имени и срока
//!   (RFC 7672, 3.1.1); DANE-TA — сертификат цепочки, от которого строится и проверяется цепочка до сертификата
//!   сервера с именем MX. Записи PKIX-TA и PKIX-EE (0, 1) не используются (RFC 7672, 3.1.3); нет пригодных —
//!   только шифрование. Ошибка поиска TLSA (кроме «нет записей») — узел пропускается (RFC 7672, 2.2).
//!   DANE приоритетнее MTA-STS.
//!
//! Источники DNS и HTTPS — типажи [`PolicyDns`] и [`PolicyHttp`] (тесты подменяют их).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mail_auth::common::parse::TxtRecordParser;
use mail_auth::hickory_resolver::TokioResolver;
use mail_auth::hickory_resolver::config::ResolverOpts;
use mail_auth::hickory_resolver::proto::dnssec::Proof;
use mail_auth::hickory_resolver::proto::rr::{RData, RecordType};
use mail_auth::mta_sts::MtaSts;
use sha2::{Digest, Sha256, Sha512};
use tokio_rustls::rustls;
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::client::danger::ServerCertVerifier;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};

use crate::config::PolicyConfig;

/// Наибольший срок политики MTA-STS, секунд (RFC 8461, 3.2).
const MAX_AGE_LIMIT: u64 = 31_557_600;

/// Режим политики MTA-STS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StsMode {
    /// Отказ от доставки на узлы вне политики и без проверенного TLS.
    Enforce,
    /// Несоответствия только в журнал.
    Testing,
    /// Политика отменена.
    None,
}

/// Политика MTA-STS домена (RFC 8461, 3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StsPolicy {
    /// Режим.
    pub mode: StsMode,
    /// Шаблоны узлов MX (`mx.example.org`, `*.example.org`), нижний регистр.
    pub mx: Vec<String>,
    /// Срок кэширования.
    pub max_age: Duration,
}

impl StsPolicy {
    /// Разбор текста политики; ошибка — политика непригодна.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut version = None;
        let mut mode = None;
        let mut max_age = None;
        let mut mx = Vec::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "version" => version = Some(value.to_owned()),
                "mode" => {
                    mode = Some(match value {
                        "enforce" => StsMode::Enforce,
                        "testing" => StsMode::Testing,
                        "none" => StsMode::None,
                        other => return Err(format!("unknown mode {other:?}")),
                    })
                }
                "max_age" => {
                    let secs: u64 = value
                        .parse()
                        .map_err(|_| format!("bad max_age {value:?}"))?;
                    max_age = Some(Duration::from_secs(secs.min(MAX_AGE_LIMIT)));
                }
                "mx" => mx.push(value.trim_end_matches('.').to_ascii_lowercase()),
                // Неизвестные поля игнорируются (RFC 8461, 3.2).
                _ => {}
            }
        }
        if version.as_deref() != Some("STSv1") {
            return Err("version is not STSv1".into());
        }
        let mode = mode.ok_or("no mode")?;
        let max_age = max_age.ok_or("no max_age")?;
        if mode != StsMode::None && mx.is_empty() {
            return Err("no mx".into());
        }
        Ok(Self { mode, mx, max_age })
    }

    /// Узел `host` разрешён политикой: точное совпадение или `*.` — ровно одна метка слева (RFC 8461, 4.1).
    pub fn allows(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.mx.iter().any(|p| match p.strip_prefix("*.") {
            Some(suffix) => host
                .strip_suffix(suffix)
                .and_then(|h| h.strip_suffix('.'))
                .is_some_and(|label| !label.is_empty() && !label.contains('.')),
            None => *p == host,
        })
    }
}

/// Запись TLSA (RFC 6698): назначение, селектор, способ сравнения, данные.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tlsa {
    /// 0 PKIX-TA, 1 PKIX-EE, 2 DANE-TA, 3 DANE-EE.
    pub usage: u8,
    /// 0 — сертификат целиком, 1 — открытый ключ (SubjectPublicKeyInfo).
    pub selector: u8,
    /// 0 — точно, 1 — SHA-256, 2 — SHA-512.
    pub matching: u8,
    /// Данные для сравнения.
    pub data: Vec<u8>,
}

impl Tlsa {
    fn usable(&self) -> bool {
        matches!(self.usage, 2 | 3) && self.selector <= 1 && self.matching <= 2
    }

    fn matches(&self, cert: &CertificateDer<'_>) -> bool {
        let selected = match self.selector {
            0 => cert.to_vec(),
            _ => match webpki::EndEntityCert::try_from(cert) {
                Ok(c) => c.subject_public_key_info().to_vec(),
                Err(_) => return false,
            },
        };
        match self.matching {
            0 => selected == self.data,
            1 => Sha256::digest(&selected).as_slice() == self.data.as_slice(),
            _ => Sha512::digest(&selected).as_slice() == self.data.as_slice(),
        }
    }
}

/// DNS для политик.
#[async_trait]
pub trait PolicyDns: Send + Sync + 'static {
    /// `id` из записи TXT `_mta-sts.<domain>`; `Ok(None)` — записи нет.
    async fn sts_id(&self, domain: &str) -> Result<Option<String>, String>;
    /// Записи TLSA `_<port>._tcp.<host>`, подтверждённые DNSSEC; `Ok(None)` — записей нет или зона не подписана.
    async fn tlsa(&self, host: &str, port: u16) -> Result<Option<Vec<Tlsa>>, String>;
}

/// Получение политики MTA-STS по HTTPS.
#[async_trait]
pub trait PolicyHttp: Send + Sync + 'static {
    /// Текст `https://mta-sts.<domain>/.well-known/mta-sts.txt`.
    async fn fetch(&self, domain: &str, cfg: &PolicyConfig) -> Result<String, String>;
}

/// DNS через hickory с проверкой DNSSEC (системные серверы).
#[derive(Clone)]
pub struct HickoryPolicyDns(TokioResolver);

impl fmt::Debug for HickoryPolicyDns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HickoryPolicyDns")
    }
}

impl HickoryPolicyDns {
    /// Резолвер по системной конфигурации с проверкой подписей DNSSEC.
    pub fn system() -> Result<Self, String> {
        let mut opts = ResolverOpts::default();
        opts.validate = true;
        let resolver = TokioResolver::builder_tokio()
            .map_err(|e| e.to_string())?
            .with_options(opts)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self(resolver))
    }
}

#[async_trait]
impl PolicyDns for HickoryPolicyDns {
    async fn sts_id(&self, domain: &str) -> Result<Option<String>, String> {
        let name = format!("_mta-sts.{}.", domain.trim_end_matches('.'));
        let lookup = match self.0.lookup(name.as_str(), RecordType::TXT).await {
            Ok(l) => l,
            Err(e) if e.is_no_records_found() || e.is_nx_domain() => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        // Несколько записей STSv1 — политики нет (RFC 8461, 3.1).
        let mut found = None;
        for r in lookup.answers() {
            let RData::TXT(txt) = &r.data else { continue };
            let raw: Vec<u8> = txt
                .txt_data
                .iter()
                .flat_map(|d| d.iter().copied())
                .collect();
            if let Ok(sts) = MtaSts::parse(&raw) {
                if found.is_some() {
                    return Ok(None);
                }
                found = Some(sts.id);
            }
        }
        Ok(found)
    }

    async fn tlsa(&self, host: &str, port: u16) -> Result<Option<Vec<Tlsa>>, String> {
        let name = format!("_{port}._tcp.{}.", host.trim_end_matches('.'));
        let lookup = match self.0.lookup(name.as_str(), RecordType::TLSA).await {
            Ok(l) => l,
            Err(e) if e.is_no_records_found() || e.is_nx_domain() => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let mut out = Vec::new();
        for r in lookup.answers() {
            let RData::TLSA(t) = &r.data else { continue };
            if r.proof != Proof::Secure {
                return Ok(None);
            }
            out.push(Tlsa {
                usage: t.cert_usage.into(),
                selector: t.selector.into(),
                matching: t.matching.into(),
                data: t.cert_data.clone(),
            });
        }
        Ok((!out.is_empty()).then_some(out))
    }
}

/// HTTPS-клиент `reqwest`: проверка сертификата, без перенаправлений.
#[derive(Debug, Clone)]
pub struct ReqwestPolicyHttp(reqwest::Client);

impl ReqwestPolicyHttp {
    /// Клиент по умолчанию.
    pub fn new() -> Result<Self, String> {
        reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map(Self)
            .map_err(|e| e.to_string())
    }
}

#[async_trait]
impl PolicyHttp for ReqwestPolicyHttp {
    async fn fetch(&self, domain: &str, cfg: &PolicyConfig) -> Result<String, String> {
        let url = format!(
            "https://mta-sts.{}/.well-known/mta-sts.txt",
            domain.trim_end_matches('.')
        );
        let mut resp = self
            .0
            .get(&url)
            .timeout(cfg.fetch_timeout)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(format!("HTTP {}", resp.status()));
        }
        let text_plain = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().to_ascii_lowercase().starts_with("text/plain"));
        if !text_plain {
            return Err("content type is not text/plain".into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            body.extend_from_slice(&chunk);
            if body.len() > cfg.max_policy_size {
                return Err("policy too large".into());
            }
        }
        String::from_utf8(body).map_err(|_| "policy is not UTF-8".into())
    }
}

/// Требование к TLS для узла MX.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostTls {
    /// По `queue.tls` (оппортунистический или обязательный).
    Default,
    /// STARTTLS с проверкой сертификата по системным корням (MTA-STS `enforce`).
    Verified,
    /// STARTTLS с проверкой по записям TLSA (DANE).
    Dane(Vec<Tlsa>),
}

struct Cached {
    id: String,
    policy: Arc<StsPolicy>,
    expires: Instant,
}

/// Политики TLS узла smtp-out: источники DNS и HTTPS, кэш MTA-STS.
pub struct TlsPolicy {
    dns: Arc<dyn PolicyDns>,
    http: Arc<dyn PolicyHttp>,
    cache: Mutex<HashMap<String, Cached>>,
}

impl fmt::Debug for TlsPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsPolicy").finish_non_exhaustive()
    }
}

impl TlsPolicy {
    /// Политики с источниками `dns` и `http`.
    pub fn new(dns: Arc<dyn PolicyDns>, http: Arc<dyn PolicyHttp>) -> Self {
        Self {
            dns,
            http,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Политики на системном DNS (с DNSSEC) и `reqwest`.
    pub fn system() -> Result<Self, String> {
        Ok(Self::new(
            Arc::new(HickoryPolicyDns::system()?),
            Arc::new(ReqwestPolicyHttp::new()?),
        ))
    }

    fn cached(&self, domain: &str, now: Instant) -> Option<(String, Arc<StsPolicy>)> {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .get(domain)
            .filter(|c| c.expires > now)
            .map(|c| (c.id.clone(), c.policy.clone()))
    }

    fn store(&self, cfg: &PolicyConfig, domain: &str, id: String, policy: Arc<StsPolicy>) {
        let now = Instant::now();
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.len() >= cfg.cache_size.max(1) && !cache.contains_key(domain) {
            cache.retain(|_, c| c.expires > now);
            if cache.len() >= cfg.cache_size.max(1) {
                return;
            }
        }
        let expires = now + policy.max_age;
        cache.insert(
            domain.to_owned(),
            Cached {
                id,
                policy,
                expires,
            },
        );
    }

    /// Действующая политика MTA-STS домена (`None` — нет или `mode: none`).
    pub async fn mta_sts(&self, cfg: &PolicyConfig, domain: &str) -> Option<Arc<StsPolicy>> {
        if !cfg.mta_sts {
            return None;
        }
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        let cached = self.cached(&domain, Instant::now());
        let policy = match self.dns.sts_id(&domain).await {
            Ok(Some(id)) => match &cached {
                Some((cid, p)) if *cid == id => Some(p.clone()),
                _ => match self.http.fetch(&domain, cfg).await {
                    Ok(text) => match StsPolicy::parse(&text) {
                        Ok(p) => {
                            let p = Arc::new(p);
                            self.store(cfg, &domain, id, p.clone());
                            Some(p)
                        }
                        Err(e) => {
                            tracing::info!(%domain, error = %e, "smtp: политика MTA-STS непригодна");
                            cached.map(|(_, p)| p)
                        }
                    },
                    Err(e) => {
                        tracing::info!(%domain, error = %e, "smtp: политика MTA-STS не получена");
                        cached.map(|(_, p)| p)
                    }
                },
            },
            Ok(None) => cached.map(|(_, p)| p),
            Err(e) => {
                tracing::info!(%domain, error = %e, "smtp: TXT MTA-STS не получена");
                cached.map(|(_, p)| p)
            }
        };
        policy.filter(|p| p.mode != StsMode::None)
    }

    /// Требование к TLS узла `host:port` с учётом политики MTA-STS `sts` домена; `Err` — узел пропускается.
    pub async fn host_tls(
        &self,
        cfg: &PolicyConfig,
        sts: Option<&StsPolicy>,
        host: &str,
        port: u16,
    ) -> Result<HostTls, String> {
        if cfg.dane {
            match self.dns.tlsa(host, port).await {
                Ok(Some(records)) => return Ok(HostTls::Dane(records)),
                Ok(None) => {}
                Err(e) => return Err(format!("{host}: TLSA lookup failed: {e}")),
            }
        }
        Ok(match sts {
            Some(p) if p.mode == StsMode::Enforce => HostTls::Verified,
            _ => HostTls::Default,
        })
    }
}

/// Сверка цепочки сертификатов сервера `chain` (первый — сертификат сервера) с записями TLSA узла `host`.
pub fn dane_verify(
    records: &[Tlsa],
    chain: &[CertificateDer<'_>],
    host: &str,
    now: UnixTime,
) -> Result<(), String> {
    let usable: Vec<&Tlsa> = records.iter().filter(|r| r.usable()).collect();
    if usable.is_empty() {
        // Пригодных записей нет: только шифрование (RFC 7672, 2.2).
        return Ok(());
    }
    let Some(leaf) = chain.first() else {
        return Err("DANE: server sent no certificate".into());
    };
    if usable.iter().any(|r| r.usage == 3 && r.matches(leaf)) {
        return Ok(());
    }
    let name = ServerName::try_from(host.trim_end_matches('.').to_owned())
        .map_err(|e| format!("DANE: bad host name: {e}"))?;
    for anchor in chain.iter().skip(1) {
        if !usable.iter().any(|r| r.usage == 2 && r.matches(anchor)) {
            continue;
        }
        let mut roots = rustls::RootCertStore::empty();
        if roots.add(anchor.clone()).is_err() {
            continue;
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let Ok(verifier) =
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build()
        else {
            continue;
        };
        if verifier
            .verify_server_cert(leaf, &chain[1..], &name, &[], now)
            .is_ok()
        {
            return Ok(());
        }
    }
    Err("DANE: certificate does not match TLSA records".into())
}

#[cfg(test)]
mod tests;
