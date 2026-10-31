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

use mail_auth::common::headers::HeaderWriter;
use mail_auth::dmarc::Policy;
use mail_auth::dmarc::verify::DmarcParameters;
use mail_auth::spf::verify::SpfParameters;
use mail_auth::{
    AuthenticatedMessage, AuthenticationResults, DmarcResult, MX, MessageAuthenticator, Parameters,
    ReceivedSpf, RecordSet, ResolverCache, Txt,
};

use crate::config::SmtpConfig;
use crate::server::Envelope;

/// Итог проверок письма.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Заголовки `Authentication-Results` и `Received-SPF` (с `\r\n`).
    pub headers: String,
    /// Доставить в «Спам»: DMARC не пройден при политике `quarantine` или `reject`.
    pub junk: bool,
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

/// Кэш DNS-записей проверок (TXT: SPF, DKIM, DMARC; MX, A, AAAA, PTR — механизмы SPF).
#[derive(Debug)]
pub struct DnsCache {
    /// TXT.
    pub txt: MemCache<Box<str>, Txt>,
    /// MX.
    pub mx: MemCache<Box<str>, RecordSet<MX>>,
    /// A.
    pub ipv4: MemCache<Box<str>, RecordSet<Ipv4Addr>>,
    /// AAAA.
    pub ipv6: MemCache<Box<str>, RecordSet<Ipv6Addr>>,
    /// PTR.
    pub ptr: MemCache<IpAddr, RecordSet<Box<str>>>,
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
type Cached<'x, P> = Parameters<
    'x,
    P,
    MemCache<Box<str>, Txt>,
    MemCache<Box<str>, RecordSet<MX>>,
    MemCache<Box<str>, RecordSet<Ipv4Addr>>,
    MemCache<Box<str>, RecordSet<Ipv6Addr>>,
    MemCache<IpAddr, RecordSet<Box<str>>>,
>;

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
        Parameters {
            params: p,
            cache_txt: Some(&self.cache.txt),
            cache_mx: Some(&self.cache.mx),
            cache_ptr: Some(&self.cache.ptr),
            cache_ipv4: Some(&self.cache.ipv4),
            cache_ipv6: Some(&self.cache.ipv6),
        }
    }

    /// Проверяет письмо `raw` транзакции `env`.
    pub async fn check(&self, env: &Envelope, raw: &[u8]) -> Outcome {
        let ip = env.conn.client.ip().to_canonical();
        let helo = env.helo.as_str();
        let mail_from = env.mail_from.as_str();
        let spf = self
            .resolver
            .verify_spf(self.params(SpfParameters::verify(ip, helo, &self.hostname, mail_from)))
            .await;
        let received_spf = ReceivedSpf::new(&spf, ip, helo, mail_from, &self.hostname).to_header();
        let ar = AuthenticationResults::new(&self.hostname);
        let Some(msg) = AuthenticatedMessage::parse(raw) else {
            let ar = ar.with_spf_mailfrom_result(&spf, ip, mail_from, helo);
            return Outcome {
                headers: ar.to_header() + &received_spf,
                junk: false,
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
            .with_dkim_results(&dkim, msg.from())
            .with_spf_mailfrom_result(&spf, ip, mail_from, helo)
            .with_arc_result(&arc, ip)
            .with_dmarc_result(&dmarc);
        Outcome {
            headers: ar.to_header() + &received_spf,
            junk,
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) mod tests;
