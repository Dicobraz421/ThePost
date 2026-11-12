//! Настройки транспорта и точка расширения правил потока почты (CONTRACTS.md#Transport; SRS 4.1, 2.6;
//! решения 2026-11-01-transport-scope-external-mta, 2026-11-01-scale-shards-geo-admin).
//!
//! Настройки транспорта — данные в основной БД PostgreSQL, а не файл узла: принятые домены и доверенные источники —
//! уровень кластера ([`ClusterTransport`]: нужны до аутентификации, чтобы выбрать организацию и источник), политики
//! доменов назначения — уровень организации ([`TenantTransport`]). Меняются через Admin API ([`TransportStore`]
//! с проверкой версии), после фиксации — сигнал [`EventKind::TransportChanged`]; узлы держат снимки в памяти и
//! перечитывают их по сигналу и при [`EventKind::Resync`].
//!
//! ```
//! use tp_contracts::transport::{DestinationPolicy, IpNet, TenantTransport, TlsRequirement};
//!
//! let net: IpNet = "192.0.2.77/24".parse()?;
//! assert_eq!(net.to_string(), "192.0.2.0/24");
//! assert!(net.contains("192.0.2.1".parse().expect("ip")));
//!
//! let policy = |pattern: &str, tls| DestinationPolicy {
//!     pattern: pattern.into(),
//!     tls,
//!     smart_host: None,
//!     egress_ip: None,
//! };
//! let snapshot = TenantTransport {
//!     version: 2,
//!     routes: vec![
//!         policy("*", TlsRequirement::Opportunistic),
//!         policy("*.partner.example", TlsRequirement::Required),
//!     ],
//!     rewrites: Vec::new(),
//! };
//! let route = snapshot.route("mx.partner.example").expect("route");
//! assert_eq!(route.pattern, "*.partner.example");
//! assert_eq!(snapshot.route("other.example").map(|p| p.pattern.as_str()), Some("*"));
//! # Ok::<(), tp_contracts::Error>(())
//! ```
//!
//! [`EventKind::TransportChanged`]: crate::bus::EventKind::TransportChanged
//! [`EventKind::Resync`]: crate::bus::EventKind::Resync

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::abuse::{in_net, mask, max_prefix};
use crate::auth::Principal;
use crate::bus::{Event, EventKind};
use crate::error::{Error, Result};
use crate::ids::{AccountId, BlobKey, DomainId, TenantId};

// ---------- Общие типы ----------

/// Подсеть `addr/prefix` с обнулёнными битами узла. В JSON и config — строка `"192.0.2.0/24"`; адрес без
/// префикса — подсеть из одного адреса (`/32`, `/128`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    /// Подсеть с обнулёнными битами узла; префикс больше длины адреса — `Invalid`.
    pub fn new(addr: IpAddr, prefix: u8) -> Result<Self> {
        if prefix > max_prefix(addr) {
            return Err(Error::invalid(format!(
                "invalid prefix /{prefix} for {addr}"
            )));
        }
        Ok(IpNet {
            addr: mask(addr, prefix),
            prefix,
        })
    }

    /// Адрес подсети.
    pub fn addr(&self) -> IpAddr {
        self.addr
    }

    /// Длина префикса.
    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// `ip` внутри подсети (семейства адресов не смешиваются).
    pub fn contains(&self, ip: IpAddr) -> bool {
        in_net(self.addr, self.prefix, ip)
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl FromStr for IpNet {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let bad = || Error::invalid(format!("invalid network {s:?}"));
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr.parse().map_err(|_| bad())?;
        let prefix = match prefix {
            Some(p) => p.parse().map_err(|_| bad())?,
            None => max_prefix(addr),
        };
        IpNet::new(addr, prefix)
    }
}

impl TryFrom<String> for IpNet {
    type Error = Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<IpNet> for String {
    fn from(n: IpNet) -> Self {
        n.to_string()
    }
}

/// Имя домена в канонической форме: ASCII в нижнем регистре без точки в конце (IDN — A-label `xn--`);
/// метки `[a-z0-9-]`, 1–63 символа, не начинаются и не заканчиваются дефисом; всего ≤ 253. Иначе — `Invalid`.
pub fn normalize_domain(name: &str) -> Result<String> {
    let bad = || Error::invalid(format!("invalid domain {name:?}"));
    let lower = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
    if lower.is_empty() || lower.len() > 253 {
        return Err(bad());
    }
    for label in lower.split('.') {
        let ok = (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return Err(bad());
        }
    }
    Ok(lower)
}

fn canonical_domain(name: &str, what: &str) -> Result<()> {
    if normalize_domain(name)? == name {
        Ok(())
    } else {
        Err(Error::invalid(format!("{what} {name:?} is not canonical")))
    }
}

/// Имя узла назначения: `host` или `host:port`, без пробелов, ≤ 255 байт; пусто — `Invalid`.
fn validate_host(host: &str, what: &str) -> Result<()> {
    if host.is_empty() || host.len() > 255 || host.chars().any(char::is_whitespace) {
        Err(Error::invalid(format!("invalid {what} {host:?}")))
    } else {
        Ok(())
    }
}

/// Имя доверенного источника: `[a-z0-9._-]`, 1–64 символа.
pub fn validate_source_name(name: &str) -> Result<()> {
    let ok = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(format!("invalid source name {name:?}")))
    }
}

/// Шаблон адреса отправителя: `user@domain` — один адрес, `@domain` — все адреса домена. Домен — в канонической
/// форме ([`normalize_domain`]); локальная часть сравнивается без учёта регистра ASCII. В JSON — строка.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AddressPattern(String);

impl AddressPattern {
    /// Проверенный шаблон; домен приводится к канонической форме, локальная часть — к нижнему регистру ASCII.
    pub fn new(pattern: &str) -> Result<Self> {
        let bad = || Error::invalid(format!("invalid address pattern {pattern:?}"));
        let (local, domain) = pattern.rsplit_once('@').ok_or_else(bad)?;
        if local.len() > 64 || local.chars().any(|c| c.is_whitespace() || c == '@') {
            return Err(bad());
        }
        let domain = normalize_domain(domain).map_err(|_| bad())?;
        Ok(AddressPattern(format!(
            "{}@{domain}",
            local.to_ascii_lowercase()
        )))
    }

    /// Шаблон как строка.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Шаблон `@domain` (все адреса домена).
    pub fn is_domain(&self) -> bool {
        self.0.starts_with('@')
    }

    /// Домен шаблона (каноническая форма).
    pub fn domain(&self) -> &str {
        self.0.rsplit_once('@').map_or("", |(_, d)| d)
    }

    /// Адрес подходит под шаблон (без учёта регистра ASCII, точка в конце домена не учитывается).
    pub fn matches(&self, address: &str) -> bool {
        let Some((local, domain)) = address.rsplit_once('@') else {
            return false;
        };
        let domain = domain.strip_suffix('.').unwrap_or(domain);
        let Some((p_local, p_domain)) = self.0.rsplit_once('@') else {
            return false;
        };
        p_domain.eq_ignore_ascii_case(domain)
            && (p_local.is_empty() || p_local.eq_ignore_ascii_case(local))
    }
}

impl fmt::Display for AddressPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AddressPattern {
    type Error = Error;

    fn try_from(s: String) -> Result<Self> {
        AddressPattern::new(&s)
    }
}

impl From<AddressPattern> for String {
    fn from(p: AddressPattern) -> Self {
        p.0
    }
}

// ---------- Уровень кластера ----------

/// Вид принятого домена (модель Exchange).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainKind {
    /// Полномочный: получатели только из каталога, неизвестный адрес отклоняется в `RCPT TO`.
    #[default]
    Authoritative,
    /// Внутренняя ретрансляция: неизвестные адреса домена передаются на узел `next_hop` (`host` или `host:port`;
    /// совместная работа со старым почтовым сервером на время миграции).
    InternalRelay {
        /// Узел назначения неизвестных адресов.
        next_hop: String,
    },
}

/// Принятый домен организации. Имя уникально в кластере: по нему smtp-in выбирает организацию получателя.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedDomain {
    /// Домен (ключ записи).
    pub id: DomainId,
    /// Организация.
    pub tenant: TenantId,
    /// Имя в канонической форме ([`normalize_domain`]).
    pub name: String,
    /// Вид.
    pub kind: DomainKind,
}

impl AcceptedDomain {
    /// Проверка полей: имя в канонической форме, `next_hop` не пуст и без пробелов. Иначе — `Invalid`.
    pub fn validate(&self) -> Result<()> {
        canonical_domain(&self.name, "domain")?;
        match &self.kind {
            DomainKind::Authoritative => Ok(()),
            DomainKind::InternalRelay { next_hop } => validate_host(next_hop, "next hop"),
        }
    }
}

/// По каким признакам соединение относится к доверенному источнику.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceMatch {
    /// Адрес клиента — в одной из подсетей (не пусто).
    Networks(Vec<IpNet>),
    /// Сертификат клиента (mTLS), проверенный по цепочке доверия слушателя, содержит имя `subject`
    /// (dNSName из SAN; без учёта регистра ASCII).
    ClientCert {
        /// Имя в сертификате.
        subject: String,
    },
}

impl SourceMatch {
    /// Насколько точно соединение подходит: `None` — не подходит; сертификат точнее любой подсети,
    /// подсеть — тем точнее, чем длиннее префикс. `cert_names` — имена проверенного сертификата клиента.
    pub fn score(&self, ip: IpAddr, cert_names: &[&str]) -> Option<u16> {
        match self {
            SourceMatch::Networks(nets) => nets
                .iter()
                .filter(|n| n.contains(ip))
                .map(|n| u16::from(n.prefix()))
                .max(),
            SourceMatch::ClientCert { subject } => cert_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(subject))
                .then_some(u16::MAX),
        }
    }
}

/// Вид доверенного источника (модель соединителей приёма Exchange).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Внешний MTA перед продуктом (SRS 2.6).
    Gateway {
        /// Принимать исходные адрес и имя клиента из `XCLIENT`/`XFORWARD`: SPF, DMARC, экран соединений и лимиты —
        /// по исходному клиенту. От источников вне списка эти команды отклоняются всегда.
        xclient: bool,
        /// При отсутствии данных об исходном клиенте принимать результаты проверок шлюза: `Authentication-Results`
        /// с этим authserv-id и ARC. `None` — результатам шлюза не доверять.
        authserv_id: Option<String>,
    },
    /// Устройство или система без SASL (МФУ, учётные системы).
    Application {
        /// Разрешённые адреса отправителя (не пусто).
        senders: Vec<AddressPattern>,
        /// Разрешены внешние получатели (иначе — только получатели принятых доменов).
        external: bool,
    },
}

/// Доверенный источник приёма. Источник приложения принадлежит организации, шлюз — кластеру или организации.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedSource {
    /// Имя (ключ записи, [`validate_source_name`]).
    pub name: String,
    /// Организация; `None` — источник кластера (общий шлюз).
    pub tenant: Option<TenantId>,
    /// Признаки соединения.
    pub matcher: SourceMatch,
    /// Вид.
    pub kind: SourceKind,
}

impl TrustedSource {
    /// Проверка полей: имя, непустые подсети, имя сертификата, отправители; источник приложения — только
    /// с организацией. Иначе — `Invalid`.
    pub fn validate(&self) -> Result<()> {
        validate_source_name(&self.name)?;
        match &self.matcher {
            SourceMatch::Networks(nets) if nets.is_empty() => {
                return Err(Error::invalid("source without networks"));
            }
            SourceMatch::ClientCert { subject } => validate_host(subject, "certificate name")?,
            SourceMatch::Networks(_) => {}
        }
        match &self.kind {
            SourceKind::Gateway {
                authserv_id: Some(id),
                ..
            } if id.is_empty() || id.chars().any(char::is_whitespace) => {
                Err(Error::invalid(format!("invalid authserv-id {id:?}")))
            }
            SourceKind::Application { .. } if self.tenant.is_none() => {
                Err(Error::invalid("application source without tenant"))
            }
            SourceKind::Application { senders, .. } if senders.is_empty() => {
                Err(Error::invalid("application source without senders"))
            }
            _ => Ok(()),
        }
    }
}

/// Снимок настроек уровня кластера: принятые домены всех организаций и доверенные источники.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClusterTransport {
    /// Версия снимка: растёт при каждом изменении уровня кластера; без записей — 0.
    pub version: u64,
    /// Принятые домены по возрастанию имени.
    pub domains: Vec<AcceptedDomain>,
    /// Доверенные источники по возрастанию имени.
    pub sources: Vec<TrustedSource>,
}

impl ClusterTransport {
    /// Принятый домен по имени (без учёта регистра ASCII, точка в конце не учитывается); поддомены — только
    /// перечисленные явно.
    pub fn domain(&self, name: &str) -> Option<&AcceptedDomain> {
        let name = name.strip_suffix('.').unwrap_or(name);
        self.domains
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name))
    }

    /// Доверенный источник соединения: наиболее точный по [`SourceMatch::score`], при равенстве — первый по имени.
    pub fn source(&self, ip: IpAddr, cert_names: &[&str]) -> Option<&TrustedSource> {
        let mut best: Option<(u16, &TrustedSource)> = None;
        for s in &self.sources {
            if let Some(score) = s.matcher.score(ip, cert_names)
                && best.is_none_or(|(b, _)| score > b)
            {
                best = Some((score, s));
            }
        }
        best.map(|(_, s)| s)
    }
}

// ---------- Уровень организации ----------

/// Требование к TLS при доставке в домен назначения.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsRequirement {
    /// По возможности; MTA-STS и DANE применяются как обычно.
    #[default]
    Opportunistic,
    /// Обязательный TLS с проверкой сертификата (цепочка и имя узла); иначе — временный отказ доставки.
    Required,
    /// Обязательный TLS с закреплённым ключом: SHA-256 SubjectPublicKeyInfo сертификата сервера — один из
    /// перечисленных (hex в нижнем регистре, 64 символа; не пусто); цепочка не проверяется.
    Pinned {
        /// Допустимые отпечатки ключей.
        spki_sha256: Vec<String>,
    },
}

/// Политика домена назначения (модель коннекторов отправки Exchange).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationPolicy {
    /// Шаблон (ключ записи в организации): `example.org` — домен, `*.example.org` — его поддомены (без самого
    /// домена), `*` — все внешние домены (relayhost). Домен — в канонической форме.
    pub pattern: String,
    /// Требование к TLS.
    pub tls: TlsRequirement,
    /// Промежуточный узел (`host` или `host:port`); `None` — доставка по MX.
    pub smart_host: Option<String>,
    /// Адрес исходящих соединений (egress-IP); `None` — адрес узла по умолчанию.
    pub egress_ip: Option<IpAddr>,
}

impl DestinationPolicy {
    /// Проверка полей: шаблон, узел, отпечатки. Иначе — `Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.pattern != "*" {
            let domain = self.pattern.strip_prefix("*.").unwrap_or(&self.pattern);
            canonical_domain(domain, "pattern")?;
        }
        if let Some(host) = &self.smart_host {
            validate_host(host, "smart host")?;
        }
        if let TlsRequirement::Pinned { spki_sha256 } = &self.tls {
            let hex = |s: &String| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            };
            if spki_sha256.is_empty() || !spki_sha256.iter().all(hex) {
                return Err(Error::invalid("invalid pinned key fingerprints"));
            }
        }
        Ok(())
    }

    /// Точность совпадения шаблона с доменом: `None` — не подходит; домен точнее поддоменов, длинный суффикс
    /// точнее короткого, `*` — наименее точен.
    pub fn specificity(&self, domain: &str) -> Option<usize> {
        let domain = domain.strip_suffix('.').unwrap_or(domain);
        if self.pattern == "*" {
            return Some(0);
        }
        if let Some(suffix) = self.pattern.strip_prefix("*.") {
            let n = domain.len();
            let sub = n > suffix.len() + 1
                && domain.is_char_boundary(n - suffix.len() - 1)
                && domain[n - suffix.len()..].eq_ignore_ascii_case(suffix)
                && domain.as_bytes()[n - suffix.len() - 1] == b'.';
            return sub.then_some(2 * suffix.len() + 1);
        }
        self.pattern
            .eq_ignore_ascii_case(domain)
            .then_some(2 * self.pattern.len() + 2)
    }
}

/// Сторона конверта.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeSide {
    /// Отправитель (`MAIL FROM`).
    Sender,
    /// Получатель (`RCPT TO`).
    Recipient,
}

/// Правило переписывания адреса конверта (модель Postfix `canonical_maps`; таблица организации, Admin API).
///
/// Отправитель конверта переписывается после проверок SPF, DKIM, DMARC (они — по исходному адресу), получатели —
/// до разрешения в каталоге; заголовки письма не меняются (это — правила потока почты, [`MailFlow`]). Одно
/// правило на адрес, без цепочек ([`TenantTransport::rewrite`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewriteRule {
    /// Исходный адрес (`old@example.org`) или домен (`@old.example`); ключ записи в организации.
    pub from: AddressPattern,
    /// Новый адрес или домен (`@new.example` — локальная часть исходного адреса сохраняется).
    pub to: AddressPattern,
    /// Переписывать отправителя конверта.
    #[serde(default = "yes")]
    pub sender: bool,
    /// Переписывать получателей.
    #[serde(default = "yes")]
    pub recipient: bool,
}

fn yes() -> bool {
    true
}

impl RewriteRule {
    /// Проверка: хотя бы одна сторона, `to` отличается от `from`. Иначе — `Invalid`.
    pub fn validate(&self) -> Result<()> {
        if !self.sender && !self.recipient {
            return Err(Error::invalid("rewrite rule applies to no side"));
        }
        if self.from == self.to {
            return Err(Error::invalid(format!(
                "rewrite rule {} maps to itself",
                self.from
            )));
        }
        Ok(())
    }

    /// Правило применяется к стороне `side`.
    pub fn applies(&self, side: EnvelopeSide) -> bool {
        match side {
            EnvelopeSide::Sender => self.sender,
            EnvelopeSide::Recipient => self.recipient,
        }
    }

    /// Адрес `address` после замены по правилу (без проверки совпадения с `from`); `None` — не адрес.
    pub fn replace(&self, address: &str) -> Option<String> {
        let (local, _) = address.rsplit_once('@')?;
        Some(if self.to.is_domain() {
            format!("{local}{}", self.to)
        } else {
            self.to.to_string()
        })
    }
}

/// Снимок настроек организации.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TenantTransport {
    /// Версия снимка: растёт при каждом изменении настроек этой организации; без записей — 0.
    pub version: u64,
    /// Политики доменов назначения по возрастанию шаблона.
    pub routes: Vec<DestinationPolicy>,
    /// Таблица переписывания адресов конверта по возрастанию `from`.
    pub rewrites: Vec<RewriteRule>,
}

impl TenantTransport {
    /// Политика для внешнего домена назначения: наиболее точный шаблон ([`DestinationPolicy::specificity`]).
    /// Вызывается только для доменов вне принятых.
    pub fn route(&self, domain: &str) -> Option<&DestinationPolicy> {
        self.routes
            .iter()
            .filter_map(|p| p.specificity(domain).map(|s| (s, p)))
            .max_by_key(|(s, _)| *s)
            .map(|(_, p)| p)
    }

    /// Политика по шаблону (маршрут правила потока, [`FlowOutcome::route`]).
    pub fn policy(&self, pattern: &str) -> Option<&DestinationPolicy> {
        self.routes.iter().find(|p| p.pattern == pattern)
    }

    /// Адрес конверта `address` после переписывания для стороны `side`; `None` — правила нет. Правило для адреса
    /// точнее правила для домена; сравнение — без учёта регистра ASCII.
    pub fn rewrite(&self, address: &str, side: EnvelopeSide) -> Option<String> {
        let candidates = || {
            self.rewrites
                .iter()
                .filter(|r| r.applies(side) && r.from.matches(address))
        };
        candidates()
            .find(|r| !r.from.is_domain())
            .or_else(|| candidates().next())
            .and_then(|r| r.replace(address))
    }

    /// Правило переписывания по ключу `from`.
    pub fn rewrite_rule(&self, from: &AddressPattern) -> Option<&RewriteRule> {
        self.rewrites.iter().find(|r| &r.from == from)
    }
}

// ---------- Хранилище ----------

/// Сигнал об изменении настроек транспорта: `tenant = None` — уровень кластера (`Event::tenant` —
/// `TenantId::DEFAULT`), иначе — организации. Реализация [`TransportStore`] публикует его после фиксации.
pub fn transport_changed(tenant: Option<TenantId>) -> Event {
    Event {
        tenant: tenant.unwrap_or(TenantId::DEFAULT),
        account: None,
        kind: EventKind::TransportChanged { tenant },
    }
}

/// Хранилище настроек транспорта (основная БД; реализация — `tp-meta`, in-memory — `tp-testkit`).
///
/// Запись — с версией снимка, на котором основано изменение (`expected`): устаревшая — `Conflict`, иначе изменение
/// применяется и возвращается новая версия (строго больше). Проверка версии — до проверки ключа (`NotFound`).
/// Поля проверяет хранилище (`validate()` типа; ошибка — `Invalid`). После фиксации реализация публикует
/// [`transport_changed`]: домены и источники — `None`, политики — организацию.
#[async_trait]
pub trait TransportStore: Send + Sync + 'static {
    /// Снимок уровня кластера.
    async fn cluster(&self) -> Result<ClusterTransport>;

    /// Снимок организации; без записей — версия 0 и пустые списки.
    async fn tenant(&self, tenant: TenantId) -> Result<TenantTransport>;

    /// Создаёт или заменяет принятый домен по `id`. Имя занято другим доменом — `Conflict`; смена организации
    /// существующего домена — `Invalid`.
    async fn put_domain(&self, expected: u64, domain: &AcceptedDomain) -> Result<u64>;

    /// Удаляет принятый домен; отсутствующий — `NotFound`.
    async fn delete_domain(&self, expected: u64, id: DomainId) -> Result<u64>;

    /// Создаёт или заменяет доверенный источник по имени. Смена организации существующего источника — `Invalid`.
    async fn put_source(&self, expected: u64, source: &TrustedSource) -> Result<u64>;

    /// Удаляет доверенный источник; отсутствующий — `NotFound`.
    async fn delete_source(&self, expected: u64, name: &str) -> Result<u64>;

    /// Создаёт или заменяет политику домена назначения организации по шаблону.
    async fn put_route(
        &self,
        tenant: TenantId,
        expected: u64,
        policy: &DestinationPolicy,
    ) -> Result<u64>;

    /// Удаляет политику по шаблону; отсутствующая — `NotFound`.
    async fn delete_route(&self, tenant: TenantId, expected: u64, pattern: &str) -> Result<u64>;

    /// Создаёт или заменяет правило переписывания адресов организации по `from` (версия организации, как у
    /// политик). По умолчанию — `Error::Internal` (хранилище без таблицы переписывания).
    async fn put_rewrite(
        &self,
        tenant: TenantId,
        expected: u64,
        rule: &RewriteRule,
    ) -> Result<u64> {
        let _ = (tenant, expected, rule);
        Err(Error::internal(
            "transport store does not store rewrite rules",
        ))
    }

    /// Удаляет правило переписывания по `from`; отсутствующее — `NotFound`. По умолчанию — `Error::Internal`.
    async fn delete_rewrite(
        &self,
        tenant: TenantId,
        expected: u64,
        from: &AddressPattern,
    ) -> Result<u64> {
        let _ = (tenant, expected, from);
        Err(Error::internal(
            "transport store does not store rewrite rules",
        ))
    }
}

// ---------- Правила потока почты ----------

/// Направление письма для правил потока.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowDirection {
    /// Приём извне (smtp-in, после Milter).
    Inbound,
    /// Отправка клиентом (submission, JMAP EmailSubmission).
    Submission,
}

/// Конверт SMTP.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FlowEnvelope {
    /// Отправитель (`MAIL FROM`); пустой — уведомление о доставке.
    pub mail_from: String,
    /// Получатели (`RCPT TO`).
    pub rcpt_to: Vec<String>,
}

/// Тело письма — объект BLOB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    /// Объект.
    pub key: BlobKey,
    /// Размер, байт.
    pub size: u64,
}

/// Письмо на входе правил потока.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowInput {
    /// Организация; письмо с получателями нескольких организаций — один вызов на организацию со своими получателями.
    pub tenant: TenantId,
    /// Направление.
    pub direction: FlowDirection,
    /// Отправитель отправки (`Submission`); при приёме — `None`.
    pub principal: Option<Principal>,
    /// Доверенный источник приёма ([`TrustedSource::name`]), если соединение от него.
    pub source: Option<String>,
    /// Конверт.
    pub envelope: FlowEnvelope,
    /// Тело (с заголовками Milter при приёме).
    pub message: BlobRef,
}

/// Итог правил потока.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowVerdict {
    /// Продолжить обработку (точки принятия решений модулей, доставка или очередь).
    #[default]
    Accept,
    /// Отклонить с текстом: SMTP 550 при приёме и submission, JMAP `forbiddenToSend` (RFC 8621).
    Reject(String),
    /// Принять и удалить без уведомления.
    Drop,
    /// Передать на модерацию (SRS 4.5) этим модераторам (не пусто).
    Moderate {
        /// Модераторы.
        moderators: Vec<AccountId>,
    },
}

/// Результат правил потока для письма.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowOutcome {
    /// Итог.
    pub verdict: FlowVerdict,
    /// Конверт после действий (копия, перенаправление); `Accept` с пустым `rcpt_to` — как `Drop`.
    pub envelope: FlowEnvelope,
    /// Изменённое тело (заголовки, тема, дисклеймер) — новый объект BLOB, записанный модулем; ссылку учитывает
    /// вызывающий. `None` — тело не менялось.
    pub message: Option<BlobRef>,
    /// Шаблон политики домена назначения организации для внешних получателей ([`TenantTransport::policy`]);
    /// `None` или нет такой политики — обычный выбор по домену.
    pub route: Option<String>,
    /// Имена сработавших правил, включая правила в режиме проверки (журнал доставки).
    pub matched: Vec<String>,
}

impl FlowOutcome {
    /// Письмо без изменений: `Accept`, исходный конверт.
    pub fn unchanged(input: &FlowInput) -> Self {
        FlowOutcome {
            verdict: FlowVerdict::Accept,
            envelope: input.envelope.clone(),
            message: None,
            route: None,
            matched: Vec::new(),
        }
    }
}

/// Точка расширения правил потока почты (модель mail flow rules Exchange). Реализация — модуль «Правила потока
/// почты» (`Registry::set_mail_flow`; правила и их хранение — в схеме модуля); без модуля не вызывается.
///
/// Вызывается один раз на письмо: smtp-in после Milter, submission и JMAP EmailSubmission — до
/// [`DecisionHooks`](crate::ext::DecisionHooks), доставки или постановки в очередь. Ошибка — ошибка операции
/// (`Unavailable` — временный отказ). Обращения к BLOB и каталогу — вне транзакций вызывающего.
#[async_trait]
pub trait MailFlow: Send + Sync + 'static {
    /// Применяет правила организации к письму.
    async fn apply(&self, input: &FlowInput) -> Result<FlowOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip")
    }

    fn policy(pattern: &str) -> DestinationPolicy {
        DestinationPolicy {
            pattern: pattern.into(),
            tls: TlsRequirement::Opportunistic,
            smart_host: None,
            egress_ip: None,
        }
    }

    #[test]
    fn object_safe() {
        fn _store(_: std::sync::Arc<dyn TransportStore>) {}
        fn _flow(_: std::sync::Arc<dyn MailFlow>) {}
    }

    #[test]
    fn ip_net() {
        let n: IpNet = "10.1.2.3/16".parse().expect("net");
        assert_eq!(n.to_string(), "10.1.0.0/16");
        assert!(n.contains(ip("10.1.255.1")) && !n.contains(ip("10.2.0.1")));
        let one: IpNet = "2001:db8::1".parse().expect("net");
        assert_eq!(one.prefix(), 128);
        assert!("10.0.0.0/33".parse::<IpNet>().is_err());
        assert!("10.0.0.0/x".parse::<IpNet>().is_err());
        assert_eq!(serde_json::to_string(&n).expect("ser"), r#""10.1.0.0/16""#);
        assert!(serde_json::from_str::<IpNet>(r#""300.0.0.0/8""#).is_err());
    }

    #[test]
    fn domains_and_patterns() {
        assert_eq!(
            normalize_domain("Example.ORG.").as_deref(),
            Ok("example.org")
        );
        for bad in ["", "a..b", "-a.org", "a-.org", "пример.рф", "a b.org"] {
            assert!(normalize_domain(bad).is_err(), "{bad}");
        }
        let one = AddressPattern::new("Scanner@Example.org").expect("pattern");
        assert_eq!(one.as_str(), "scanner@example.org");
        assert!(one.matches("SCANNER@example.ORG.") && !one.matches("other@example.org"));
        let all = AddressPattern::new("@example.org").expect("pattern");
        assert!(all.matches("any@example.org") && !all.matches("any@sub.example.org"));
        assert!(AddressPattern::new("nobody").is_err());
        assert!(AddressPattern::new("a b@example.org").is_err());
        assert!(serde_json::from_str::<AddressPattern>(r#""x@-bad""#).is_err());
    }

    #[test]
    fn cluster_lookup() {
        let tenant = TenantId::new();
        let domain = AcceptedDomain {
            id: DomainId::new(),
            tenant,
            name: "example.org".into(),
            kind: DomainKind::Authoritative,
        };
        let source = |name: &str, matcher| TrustedSource {
            name: name.into(),
            tenant: None,
            matcher,
            kind: SourceKind::Gateway {
                xclient: true,
                authserv_id: None,
            },
        };
        let wide = source(
            "a-wide",
            SourceMatch::Networks(vec!["192.0.2.0/24".parse().expect("net")]),
        );
        let narrow = source(
            "b-narrow",
            SourceMatch::Networks(vec!["192.0.2.0/28".parse().expect("net")]),
        );
        let cert = source(
            "c-cert",
            SourceMatch::ClientCert {
                subject: "gw.example.org".into(),
            },
        );
        let snap = ClusterTransport {
            version: 3,
            domains: vec![domain.clone()],
            sources: vec![wide.clone(), narrow.clone(), cert.clone()],
        };
        assert_eq!(snap.domain("EXAMPLE.org."), Some(&domain));
        assert_eq!(snap.domain("sub.example.org"), None);
        assert_eq!(snap.source(ip("192.0.2.200"), &[]), Some(&wide));
        assert_eq!(snap.source(ip("192.0.2.5"), &[]), Some(&narrow));
        assert_eq!(
            snap.source(ip("192.0.2.5"), &["GW.example.org"]),
            Some(&cert)
        );
        assert_eq!(snap.source(ip("198.51.100.1"), &[]), None);
    }

    #[test]
    fn validation() {
        let mut d = AcceptedDomain {
            id: DomainId::new(),
            tenant: TenantId::new(),
            name: "Example.org".into(),
            kind: DomainKind::Authoritative,
        };
        assert!(d.validate().is_err());
        d.name = "example.org".into();
        assert_eq!(d.validate(), Ok(()));
        d.kind = DomainKind::InternalRelay {
            next_hop: String::new(),
        };
        assert!(d.validate().is_err());

        let mut s = TrustedSource {
            name: "mfu".into(),
            tenant: None,
            matcher: SourceMatch::Networks(vec!["10.0.0.0/8".parse().expect("net")]),
            kind: SourceKind::Application {
                senders: vec![AddressPattern::new("scanner@example.org").expect("p")],
                external: false,
            },
        };
        assert!(s.validate().is_err(), "application without tenant");
        s.tenant = Some(TenantId::new());
        assert_eq!(s.validate(), Ok(()));
        s.name = "Bad Name".into();
        assert!(s.validate().is_err());
        s.name = "mfu".into();
        s.matcher = SourceMatch::Networks(vec![]);
        assert!(s.validate().is_err());

        assert_eq!(policy("*").validate(), Ok(()));
        assert_eq!(policy("*.partner.example").validate(), Ok(()));
        assert!(policy("*.").validate().is_err());
        assert!(policy("Partner.example").validate().is_err());
        let mut p = policy("partner.example");
        p.tls = TlsRequirement::Pinned {
            spki_sha256: vec!["ab".repeat(32)],
        };
        assert_eq!(p.validate(), Ok(()));
        p.tls = TlsRequirement::Pinned {
            spki_sha256: vec!["AB".repeat(32)],
        };
        assert!(p.validate().is_err());
        p.tls = TlsRequirement::Required;
        p.smart_host = Some("relay .example".into());
        assert!(p.validate().is_err());
    }

    #[test]
    fn route_specificity() {
        let snap = TenantTransport {
            version: 1,
            routes: vec![
                policy("*"),
                policy("*.example.org"),
                policy("*.eu.example.org"),
                policy("example.org"),
            ],
            rewrites: Vec::new(),
        };
        let route = |d: &str| snap.route(d).map(|p| p.pattern.as_str());
        assert_eq!(route("example.org"), Some("example.org"));
        assert_eq!(route("Mail.Example.org."), Some("*.example.org"));
        assert_eq!(route("mx.eu.example.org"), Some("*.eu.example.org"));
        assert_eq!(route("eu.example.org"), Some("*.example.org"));
        assert_eq!(route("badexample.org"), Some("*"));
        assert!(snap.policy("*.example.org").is_some() && snap.policy("x").is_none());
        assert_eq!(TenantTransport::default().route("example.org"), None);
    }

    #[test]
    fn rewrite_table() {
        let rule = |from: &str, to: &str, sender, recipient| RewriteRule {
            from: AddressPattern::new(from).expect("from"),
            to: AddressPattern::new(to).expect("to"),
            sender,
            recipient,
        };
        let snap = TenantTransport {
            rewrites: vec![
                rule("@legacy.example", "@example.org", false, true),
                rule("@old.example", "@example.org", true, true),
                rule("boss@old.example", "ceo@example.org", true, true),
            ],
            ..TenantTransport::default()
        };
        let rw = |a: &str, side| snap.rewrite(a, side);
        assert_eq!(
            rw("Boss@Old.example", EnvelopeSide::Sender).as_deref(),
            Some("ceo@example.org")
        );
        assert_eq!(
            rw("Ann@OLD.example", EnvelopeSide::Recipient).as_deref(),
            Some("Ann@example.org")
        );
        assert_eq!(
            rw("x@legacy.example", EnvelopeSide::Recipient).as_deref(),
            Some("x@example.org")
        );
        assert_eq!(rw("x@legacy.example", EnvelopeSide::Sender), None);
        assert_eq!(rw("x@other.example", EnvelopeSide::Sender), None);
        assert_eq!(rw("", EnvelopeSide::Sender), None);
        assert!(
            snap.rewrite_rule(&AddressPattern::new("@OLD.example").expect("p"))
                .is_some()
        );

        assert_eq!(
            rule("@a.example", "@b.example", true, false).validate(),
            Ok(())
        );
        assert!(
            rule("@a.example", "@b.example", false, false)
                .validate()
                .is_err()
        );
        assert!(
            rule("x@a.example", "X@a.example", true, true)
                .validate()
                .is_err()
        );
        let json = r#"{"from":"@Old.Example","to":"@example.org"}"#;
        let parsed: RewriteRule = serde_json::from_str(json).expect("de");
        assert_eq!(parsed, rule("@old.example", "@example.org", true, true));
    }

    #[test]
    fn flow_shapes() {
        let input = FlowInput {
            tenant: TenantId::DEFAULT,
            direction: FlowDirection::Submission,
            principal: None,
            source: None,
            envelope: FlowEnvelope {
                mail_from: "a@example.org".into(),
                rcpt_to: vec!["b@example.net".into()],
            },
            message: BlobRef {
                key: BlobKey {
                    tenant: TenantId::DEFAULT,
                    hash: crate::ids::BlobHash([7; 32]),
                    generation: crate::ids::Gen(1),
                },
                size: 10,
            },
        };
        let out = FlowOutcome::unchanged(&input);
        assert_eq!(out.verdict, FlowVerdict::Accept);
        assert_eq!(out.envelope, input.envelope);
        assert_eq!(
            serde_json::to_string(&FlowVerdict::Reject("no".into())).expect("ser"),
            r#"{"reject":"no"}"#
        );
        let ev = transport_changed(None);
        assert_eq!(ev.tenant, TenantId::DEFAULT);
        assert!(ev.kind.is_system());
        let t = TenantId::new();
        assert_eq!(transport_changed(Some(t)).tenant, t);
    }
}
