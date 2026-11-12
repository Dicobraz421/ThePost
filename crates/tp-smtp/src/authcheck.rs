//! Аутентификация входящей почты (SRS 4.1): SPF (RFC 7208), DKIM (RFC 6376), DMARC (RFC 7489), ARC (RFC 8617)
//! на `mail-auth` с DNS-резолвером hickory и кэшем записей.
//!
//! Результат — заголовки `Authentication-Results` (RFC 8601) и `Received-SPF`, добавляемые к письму перед
//! доставкой, и признак «в Спам»: проверка DMARC не прошла, а политика домена отправителя — `quarantine` или
//! `reject`. Письмо не отклоняется: решение о нём — у получателя (папка «Спам»).

use std::borrow::Borrow;
use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::Instant;

use mail_auth::dmarc::Policy;
use mail_auth::dmarc::verify::DmarcParameters;
use mail_auth::headers::HeaderWriter;
use mail_auth::spf::verify::SpfParameters;
use mail_auth::{
    AuthenticatedMessage, AuthenticationResults, DmarcResult, MessageAuthenticator, Mx, Negative,
    Parameters, ReceivedSpf, RecordSet, ResolverCache, TxtRecord,
};

use crate::config::SmtpConfig;
use crate::forward::Sealer;
use crate::server::Envelope;

/// Итог проверок письма.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Заголовки `Authentication-Results` и `Received-SPF` (с `\r\n`).
    pub headers: String,
    /// Доставить в «Спам»: DMARC не пройден при политике `quarantine` или `reject`.
    pub junk: bool,
    /// DMARC пройден: адресу `From` можно доверять (приём iMIP, SMTP-33).
    pub dmarc_pass: bool,
}

/// Кэш записей одного типа: значение до срока из ответа DNS; при переполнении вытесняются истёкшие,
/// затем — всё.
pub struct MemCache<K, V> {
    map: Mutex<HashMap<K, (V, Instant)>>,
    capacity: usize,
}

impl<K, V> MemCache<K, V> {
    fn new(capacity: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
        }
    }
}

impl<K, V> fmt::Debug for MemCache<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemCache")
            .field("len", &lock(&self.map).len())
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl<K: Hash + Eq, V: Clone> ResolverCache<K, V> for MemCache<K, V> {
    fn get<Q>(&self, name: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let map = lock(&self.map);
        map.get(name)
            .filter(|(_, until)| *until > Instant::now())
            .map(|(v, _)| v.clone())
    }

    fn remove<Q>(&self, name: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        lock(&self.map).remove(name).map(|(v, _)| v)
    }

    fn insert(&self, key: K, value: V, valid_until: Instant) {
        let mut map = lock(&self.map);
        if map.len() >= self.capacity {
            let now = Instant::now();
            map.retain(|_, (_, until)| *until > now);
            if map.len() >= self.capacity {
                map.clear();
            }
        }
        map.insert(key, (value, valid_until));
    }
}

/// Ответ DNS в кэше: записи или отрицательный ответ (RFC 2308).
pub type Answer<T> = Result<RecordSet<T>, Negative>;

/// Кэш DNS-записей проверок (TXT: SPF, DKIM, DMARC; MX, A, AAAA, PTR — механизмы SPF).
#[derive(Debug)]
pub struct DnsCache {
    /// TXT.
    pub txt: MemCache<Box<str>, TxtRecord>,
    /// MX.
    pub mx: MemCache<Box<str>, Answer<Mx>>,
    /// A.
    pub ipv4: MemCache<Box<str>, Answer<Ipv4Addr>>,
    /// AAAA.
    pub ipv6: MemCache<Box<str>, Answer<Ipv6Addr>>,
    /// PTR.
    pub ptr: MemCache<IpAddr, Answer<Box<str>>>,
}

impl mail_auth::DnsCache for DnsCache {
    type Txt = MemCache<Box<str>, TxtRecord>;
    type Mx = MemCache<Box<str>, Answer<Mx>>;
    type Ipv4 = MemCache<Box<str>, Answer<Ipv4Addr>>;
    type Ipv6 = MemCache<Box<str>, Answer<Ipv6Addr>>;
    type Ptr = MemCache<IpAddr, Answer<Box<str>>>;

    fn txt(&self) -> Option<&Self::Txt> {
        Some(&self.txt)
    }

    fn mx(&self) -> Option<&Self::Mx> {
        Some(&self.mx)
    }

    fn ipv4(&self) -> Option<&Self::Ipv4> {
        Some(&self.ipv4)
    }

    fn ipv6(&self) -> Option<&Self::Ipv6> {
        Some(&self.ipv6)
    }

    fn ptr(&self) -> Option<&Self::Ptr> {
        Some(&self.ptr)
    }
}

impl DnsCache {
    /// Кэш до `capacity` записей каждого типа.
    pub fn new(capacity: usize) -> Self {
        Self {
            txt: MemCache::new(capacity),
            mx: MemCache::new(capacity),
            ipv4: MemCache::new(capacity),
            ipv6: MemCache::new(capacity),
            ptr: MemCache::new(capacity),
        }
    }
}

/// Проверки SPF, DKIM, DMARC и ARC входящей почты.
pub struct AuthCheck {
    resolver: MessageAuthenticator,
    hostname: String,
    cache: DnsCache,
}

impl fmt::Debug for AuthCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthCheck")
            .field("hostname", &self.hostname)
            .finish_non_exhaustive()
    }
}

/// Параметры проверки с кэшами `DnsCache`.
type Cached<'x, P> = Parameters<'x, P, DnsCache>;

impl AuthCheck {
    /// Проверки через резолвер `resolver` (системный — [`MessageAuthenticator::new_system_conf`]).
    pub fn new(resolver: MessageAuthenticator, cfg: &SmtpConfig) -> Self {
        Self {
            resolver,
            hostname: cfg.hostname.clone(),
            cache: DnsCache::new(cfg.authcheck.dns_cache_size),
        }
    }

    /// Кэш DNS (тесты заполняют его записями вместо сети).
    pub fn cache(&self) -> &DnsCache {
        &self.cache
    }

    fn params<'x, P>(&'x self, p: P) -> Cached<'x, P> {
        Parameters::new(p).with_cache(&self.cache)
    }

    /// Проверяет письмо `raw` транзакции `env`.
    pub async fn check(&self, env: &Envelope, raw: &[u8]) -> Outcome {
        let ip = env.conn.client.ip().to_canonical();
        let helo = env.helo.as_str();
        let mail_from = env.mail_from.as_str();
        let spf_params = SpfParameters::helo_and_mail_from(ip, helo, &self.hostname, mail_from);
        let spf = self.resolver.verify_spf(self.params(spf_params)).await;
        let received_spf = ReceivedSpf::new(&spf, &spf_params).to_header();
        let ar = AuthenticationResults::new(&self.hostname);
        let Some(msg) = AuthenticatedMessage::parse(raw) else {
            let ar = ar.with_spf_result(&spf, &spf_params);
            return Outcome {
                headers: ar.to_header() + &received_spf,
                junk: false,
                dmarc_pass: false,
            };
        };
        let dkim = self.resolver.verify_dkim(self.params(&msg)).await;
        let arc = self.resolver.verify_arc(self.params(&msg)).await;
        let mail_from_domain = mail_from.rsplit_once('@').map_or(helo, |(_, d)| d);
        let dmarc = self
            .resolver
            .verify_dmarc(self.params(DmarcParameters::new(&msg, &dkim, mail_from_domain, &spf)))
            .await;
        let junk = matches!(dmarc.result(), DmarcResult::Fail(_))
            && matches!(dmarc.policy(), Policy::Quarantine | Policy::Reject);
        let ar = ar
            .with_dkim_results(&dkim, msg.first_from_address())
            .with_spf_result(&spf, &spf_params)
            .with_arc_result(&arc, ip)
            .with_dmarc_result(&dmarc);
        Outcome {
            headers: ar.to_header() + &received_spf,
            junk,
            dmarc_pass: matches!(dmarc.result(), DmarcResult::Pass),
        }
    }
}

impl AuthCheck {
    /// Результаты проверок шлюза (SMTP-20) — письмо от доверенного шлюза без данных об исходном клиенте:
    /// верхний заголовок `Authentication-Results` с authserv-id `authserv_id`, иначе — `ARC-Authentication-Results`
    /// последнего звена цепочки ARC, если цепочка проходит проверку и звено поставил этот authserv-id. Признак
    /// «в Спам» — DMARC `fail` при политике `quarantine` или `reject` в результатах шлюза. Заголовки не
    /// добавляются (результаты шлюза уже в письме). `None` — результатов шлюза нет: проверки — как обычно.
    pub async fn gateway(&self, raw: &[u8], authserv_id: &str) -> Option<Outcome> {
        let results = match trusted_results(raw, authserv_id) {
            Some(r) => r,
            None => {
                let msg = AuthenticatedMessage::parse(raw)?;
                let arc = self.resolver.verify_arc(self.params(&msg)).await;
                if !matches!(arc.result(), mail_auth::DkimResult::Pass) {
                    return None;
                }
                let last = arc.chain().last()?;
                let value = String::from_utf8_lossy(last.results.value).into_owned();
                // `i=N; authserv-id; результаты`.
                let (_, rest) = value.split_once(';')?;
                let (id, results) = rest.split_once(';').unwrap_or((rest, ""));
                if !id.trim().eq_ignore_ascii_case(authserv_id) {
                    return None;
                }
                results.to_owned()
            }
        };
        let lower = results.to_ascii_lowercase();
        let has = |token: &str| {
            lower
                .split([';', ' ', '\t', '\r', '\n'])
                .any(|t| t == token)
        };
        let junk = has("dmarc=fail")
            && (lower.contains("policy.dmarc=quarantine") || lower.contains("policy.dmarc=reject"));
        Some(Outcome {
            headers: String::new(),
            junk,
            dmarc_pass: has("dmarc=pass"),
        })
    }

    /// Заголовки набора ARC (RFC 8617) узла для пересылаемого письма `raw` (с `\r\n`); `ip` — адрес клиента,
    /// от которого письмо принято (неизвестен — не указывается). Цепочка письма уже нарушена — `None`.
    pub(crate) async fn seal(
        &self,
        sealer: &Sealer,
        raw: &[u8],
        ip: Option<IpAddr>,
    ) -> Option<String> {
        let msg = AuthenticatedMessage::parse(raw)?;
        let dkim = self.resolver.verify_dkim(self.params(&msg)).await;
        let arc = self.resolver.verify_arc(self.params(&msg)).await;
        let ar = AuthenticationResults::new(&self.hostname)
            .with_dkim_results(&dkim, msg.first_from_address())
            .with_arc_result(&arc, ip.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        match sealer.seal(&msg, &ar, &arc) {
            Ok(set) => Some(set.to_header()),
            Err(e) => {
                tracing::info!(error = %e, "smtp: пересылаемое письмо не подписано ARC");
                None
            }
        }
    }
}

/// Результаты верхнего заголовка `Authentication-Results` с authserv-id `id` (без него).
fn trusted_results(raw: &[u8], id: &str) -> Option<String> {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..head_end]);
    // Развёрнутые заголовки: строка продолжения начинается с пробела или табуляции.
    let mut headers: Vec<String> = Vec::new();
    for line in head.split("\r\n") {
        match headers.last_mut() {
            Some(h) if line.starts_with([' ', '\t']) => {
                h.push(' ');
                h.push_str(line.trim());
            }
            _ => headers.push(line.to_owned()),
        }
    }
    headers.iter().find_map(|h| {
        let (name, value) = h.split_once(':')?;
        if !name.trim().eq_ignore_ascii_case("Authentication-Results") {
            return None;
        }
        let (serv, results) = value.split_once(';').unwrap_or((value, ""));
        // authserv-id может сопровождаться версией (`id 1`).
        let serv = serv.split_whitespace().next()?;
        serv.eq_ignore_ascii_case(id).then(|| results.to_owned())
    })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) mod tests;
