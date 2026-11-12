//! `thepost doctor`: DNS почтового домена, сертификаты и PROXY v2 (SRS 5.5; APP-06).
//!
//! Проверки: MX основного домена (и указывает ли он на `smtp.hostname`), SPF, DKIM по селекторам
//! `smtp.dkim.domains`, DMARC, MTA-STS, PTR исходящих адресов (`smtp.queue.source_ip` или адреса
//! `smtp.hostname`), срок и имена сертификата `tls`, закрытый ключ `tls` (файл `key_file` или секрет `tls.key`
//! подходит к цепочке), слушатели с PROXY protocol v2 за балансировщиком.
//! Имена зон RFC 6761 (`.test`, `.example`, `.invalid`, `.localhost`) — проверки DNS пропускаются (SKIP, CMP-09).
//! При роли dav — SRV `_caldavs._tcp` и `_carddavs._tcp` с TXT `path=` (RFC 6764) основного домена и доменов DKIM.
//! DNS — через [`Dns`]: в работе — системный резолвер (`mail-auth`), в тестах — подстановка.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use mail_auth::{MessageAuthenticator, NoCache};
use tp_contracts::secrets::SecretStore;
use tp_platform::listen::EndpointConfig;
use tp_platform::{AcmeConfig, Config, TlsCerts, TlsConfig};
use tp_smtp::SmtpConfig;

use crate::doctor::{Report, Status, message};
use crate::sections::{self, DoctorSection, IMAP, SMTP};

/// Запись SRV (RFC 2782); узел — без точки в конце, пустой — «.» (служба недоступна).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SrvRecord {
    /// Приоритет.
    pub priority: u16,
    /// Вес.
    pub weight: u16,
    /// Порт.
    pub port: u16,
    /// Узел.
    pub target: String,
}

/// Запросы DNS проверок. Нет записей (NXDOMAIN, пустой ответ) — пустой список; сбой — `Err`.
#[async_trait]
pub trait Dns: Send + Sync {
    /// MX: (предпочтение, узел) в порядке предпочтения.
    async fn mx(&self, domain: &str) -> Result<Vec<(u16, String)>, String>;
    /// SRV имени в порядке приоритета.
    async fn srv(&self, name: &str) -> Result<Vec<SrvRecord>, String>;
    /// Текст записей TXT имени.
    async fn txt(&self, name: &str) -> Result<Vec<String>, String>;
    /// Адреса узла (A, затем AAAA).
    async fn ip(&self, host: &str) -> Result<Vec<IpAddr>, String>;
    /// Имена PTR адреса.
    async fn ptr(&self, ip: IpAddr) -> Result<Vec<String>, String>;
}

/// Системный резолвер (`/etc/resolv.conf`).
pub struct SystemDns(MessageAuthenticator);

impl std::fmt::Debug for SystemDns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SystemDns")
    }
}

impl SystemDns {
    /// Резолвер по системной конфигурации.
    pub fn new() -> tp_contracts::Result<Self> {
        crate::services::system_resolver().map(Self)
    }
}

/// Сбой запроса; «записи нет» — пустой ответ.
fn absent<T>(r: mail_auth::Result<Vec<T>>) -> Result<Vec<T>, String> {
    match r {
        Ok(v) => Ok(v),
        Err(mail_auth::Error::Dns(mail_auth::DnsError::RecordNotFound(_))) => Ok(Vec::new()),
        Err(e) => Err(e.to_string()),
    }
}

#[async_trait]
impl Dns for SystemDns {
    async fn mx(&self, domain: &str) -> Result<Vec<(u16, String)>, String> {
        let r = self.0.mx_lookup(domain, None::<&NoCache>).await.map(|set| {
            let mut out: Vec<(u16, String)> = set
                .records
                .iter()
                .flat_map(|mx| {
                    mx.exchanges
                        .iter()
                        .map(move |h| (mx.preference, h.trim_end_matches('.').to_owned()))
                })
                .collect();
            out.sort();
            out
        });
        absent(r)
    }

    async fn srv(&self, name: &str) -> Result<Vec<SrvRecord>, String> {
        // mail-auth SRV не оборачивает: запрос — его резолвером hickory.
        use mail_auth::hickory_resolver::proto::rr::RData;
        match self.0.resolver().srv_lookup(name).await {
            Ok(lookup) => {
                let mut out: Vec<SrvRecord> = lookup
                    .answers()
                    .iter()
                    .filter_map(|r| match &r.data {
                        RData::SRV(s) => Some(SrvRecord {
                            priority: s.priority,
                            weight: s.weight,
                            port: s.port,
                            target: s.target.to_string().trim_end_matches('.').to_owned(),
                        }),
                        _ => None,
                    })
                    .collect();
                out.sort();
                Ok(out)
            }
            Err(e) if e.is_no_records_found() => Ok(Vec::new()),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn txt(&self, name: &str) -> Result<Vec<String>, String> {
        // Записи имени склеиваются резолвером: признаки (`v=spf1` и т. п.) ищутся в общем тексте.
        let r = self
            .0
            .txt_raw_lookup(name)
            .await
            .map(|raw| vec![String::from_utf8_lossy(&raw).into_owned()]);
        absent(r).map(|v| v.into_iter().filter(|t| !t.is_empty()).collect())
    }

    async fn ip(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        absent(
            self.0
                .ip_lookup(
                    host,
                    mail_auth::IpLookupStrategy::Ipv4thenIpv6,
                    16,
                    None::<&NoCache>,
                    None::<&NoCache>,
                )
                .await,
        )
    }

    async fn ptr(&self, ip: IpAddr) -> Result<Vec<String>, String> {
        let r = self.0.ptr_lookup(ip, None::<&NoCache>).await.map(|set| {
            set.records
                .iter()
                .map(|h| h.trim_end_matches('.').to_owned())
                .collect()
        });
        absent(r)
    }
}

/// Запрос с пределом ожидания `timeout`.
async fn query<T>(
    timeout: Duration,
    f: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(timeout, f).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "нет ответа за {}",
            tp_contracts::config::duration::format(timeout)
        )),
    }
}

/// Имена DNS без учёта регистра и точки в конце.
fn same_host(a: &str, b: &str) -> bool {
    a.trim_end_matches('.')
        .eq_ignore_ascii_case(b.trim_end_matches('.'))
}

/// Запись TXT с признаком `tag` (`v=spf1`, `v=DMARC1`, …) без учёта регистра.
fn tagged<'a>(records: &'a [String], tag: &str) -> Option<&'a String> {
    let tag = tag.to_ascii_lowercase();
    records
        .iter()
        .find(|r| r.to_ascii_lowercase().contains(&tag))
}

/// Зоны для тестов и примеров (RFC 6761): в публичном DNS записей у их имён нет и не будет.
const RESERVED_ZONES: [&str; 4] = ["test", "example", "invalid", "localhost"];

/// Зона RFC 6761, в которой лежит имя `name`.
fn reserved_zone(name: &str) -> Option<&'static str> {
    let tld = name.trim_end_matches('.').rsplit('.').next()?;
    RESERVED_ZONES
        .into_iter()
        .find(|z| tld.eq_ignore_ascii_case(z))
}

/// Имя `name` — в зарезервированной зоне: проверка `id` пропущена (`true`).
fn skip_reserved(id: &str, name: &str, report: &mut Report) -> bool {
    let Some(zone) = reserved_zone(name) else {
        return false;
    };
    report.push(
        id,
        Status::Skipped,
        name,
        format!(
            "зона .{zone} зарезервирована (RFC 6761): в публичном DNS записей нет, проверка пропущена"
        ),
    );
    true
}

/// Все проверки DNS, сертификатов и PROXY v2; `store` — хранилище секретов узла (ключ `tls.key`), `None` — не
/// настроено.
pub async fn check(
    cfg: &Config,
    dns: &dyn Dns,
    store: Option<&Arc<dyn SecretStore>>,
    doctor: &DoctorSection,
    report: &mut Report,
) {
    let smtp = cfg
        .section::<SmtpConfig>(SMTP)
        .map(|s| (*s).clone())
        .unwrap_or_default();
    let domain = cfg.domain().trim_end_matches('.').to_ascii_lowercase();
    let t = doctor.dns_timeout;
    mx(&domain, &smtp, dns, t, report).await;
    spf(&domain, dns, t, report).await;
    dkim(&smtp, dns, t, report).await;
    dmarc(&domain, dns, t, report).await;
    mta_sts(&domain, dns, t, report).await;
    ptr(&smtp, dns, t, report).await;
    dav_srv(cfg, &domain, &smtp, dns, t, report).await;
    certificate(cfg, &smtp, store, doctor, SystemTime::now(), report).await;
    proxy(cfg, &smtp, report);
}

async fn mx(domain: &str, smtp: &SmtpConfig, dns: &dyn Dns, t: Duration, report: &mut Report) {
    let id = "dns.mx";
    if skip_reserved(id, domain, report) {
        return;
    }
    match query(t, dns.mx(domain)).await {
        Err(e) => report.push(id, Status::Fail, domain, format!("MX не получены: {e}")),
        Ok(list) if list.is_empty() => report.push(
            id,
            Status::Fail,
            domain,
            "нет записей MX: внешняя почта на домен не будет доставляться",
        ),
        Ok(list) if list.iter().all(|(_, h)| h.is_empty()) => report.push(
            id,
            Status::Fail,
            domain,
            "null MX (RFC 7505): домен объявил, что почту не принимает",
        ),
        Ok(list) => {
            let value = list
                .iter()
                .map(|(p, h)| format!("{p} {h}"))
                .collect::<Vec<_>>()
                .join(", ");
            if list.iter().any(|(_, h)| same_host(h, &smtp.hostname)) {
                report.push(id, Status::Ok, value, "MX указывает на этот узел");
            } else {
                report.push(
                    id,
                    Status::Warn,
                    value,
                    format!(
                        "MX не указывает на smtp.hostname ({}): проверьте, что узлы MX — шлюз перед этим узлом",
                        smtp.hostname
                    ),
                );
            }
        }
    }
}

async fn spf(domain: &str, dns: &dyn Dns, t: Duration, report: &mut Report) {
    let id = "dns.spf";
    if skip_reserved(id, domain, report) {
        return;
    }
    match query(t, dns.txt(domain)).await {
        Err(e) => report.push(id, Status::Fail, domain, format!("TXT не получены: {e}")),
        Ok(list) => match tagged(&list, "v=spf1") {
            None => report.push(
                id,
                Status::Warn,
                domain,
                "нет записи SPF: получатели не смогут проверить отправителя (DMARC не пройдёт по SPF)",
            ),
            Some(r) if r.contains("+all") => report.push(
                id,
                Status::Warn,
                r.clone(),
                "SPF «+all» разрешает отправку от имени домена с любого адреса",
            ),
            Some(r) => report.push(id, Status::Ok, r.clone(), "запись SPF опубликована"),
        },
    }
}

async fn dkim(smtp: &SmtpConfig, dns: &dyn Dns, t: Duration, report: &mut Report) {
    if smtp.dkim.domains.is_empty() {
        report.push(
            "dns.dkim",
            Status::Warn,
            "",
            "подпись DKIM не настроена (smtp.dkim.domains): письма домена хуже проходят проверки получателей",
        );
        return;
    }
    // Письмо подписывается всеми ключами домена или селектором политики организации (SMTP-40): опубликованы
    // должны быть все ключи.
    for (domain, d) in &smtp.dkim.domains {
        let id = format!("dns.dkim.{domain}");
        let names: Vec<String> = d
            .keys
            .keys()
            .map(|s| format!("{s}._domainkey.{domain}"))
            .collect();
        let Some(first) = names.first() else {
            report.push(
                &id,
                Status::Warn,
                "",
                "у домена нет ключей DKIM (smtp.dkim.domains.*.keys): письма не подписываются",
            );
            continue;
        };
        if skip_reserved(&id, first, report) {
            continue;
        }
        let mut missing = Vec::new();
        let mut failed = None;
        for name in &names {
            match query(t, dns.txt(name)).await {
                Err(e) => {
                    failed = Some((name.clone(), e));
                    break;
                }
                Ok(list) if list.iter().any(|r| r.contains("p=")) => {}
                Ok(_) => missing.push(name.clone()),
            }
        }
        if let Some((name, e)) = failed {
            report.push(&id, Status::Fail, name, format!("TXT не получены: {e}"));
        } else if !missing.is_empty() {
            report.push(
                &id,
                Status::Fail,
                missing.join(", "),
                "открытый ключ селектора не опубликован: подписанные письма не пройдут проверку DKIM",
            );
        } else {
            report.push(
                &id,
                Status::Ok,
                names.join(", "),
                "открытые ключи опубликованы",
            );
        }
    }
}

async fn dmarc(domain: &str, dns: &dyn Dns, t: Duration, report: &mut Report) {
    let id = "dns.dmarc";
    let name = format!("_dmarc.{domain}");
    if skip_reserved(id, &name, report) {
        return;
    }
    match query(t, dns.txt(&name)).await {
        Err(e) => report.push(id, Status::Fail, name, format!("TXT не получены: {e}")),
        Ok(list) => match tagged(&list, "v=dmarc1") {
            Some(r) => report.push(id, Status::Ok, r.clone(), "политика DMARC опубликована"),
            None => report.push(
                id,
                Status::Warn,
                name,
                "нет политики DMARC: подделку адреса домена получатели не отклонят",
            ),
        },
    }
}

async fn mta_sts(domain: &str, dns: &dyn Dns, t: Duration, report: &mut Report) {
    let id = "dns.mta_sts";
    let name = format!("_mta-sts.{domain}");
    if skip_reserved(id, &name, report) {
        return;
    }
    match query(t, dns.txt(&name)).await {
        Err(e) => report.push(id, Status::Fail, name, format!("TXT не получены: {e}")),
        Ok(list) => match tagged(&list, "v=stsv1") {
            Some(r) => report.push(
                id,
                Status::Ok,
                r.clone(),
                format!("политика — https://mta-sts.{domain}/.well-known/mta-sts.txt"),
            ),
            None => report.push(
                id,
                Status::Warn,
                name,
                "нет MTA-STS: отправители не обязаны шифровать почту на домен (RFC 8461)",
            ),
        },
    }
}

async fn ptr(smtp: &SmtpConfig, dns: &dyn Dns, t: Duration, report: &mut Report) {
    let id = "dns.ptr";
    let host = smtp.hostname.trim_end_matches('.');
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        report.push(
            id,
            Status::Warn,
            host,
            "smtp.hostname не задан: приветствие и PTR не совпадут, почту узла примут за спам",
        );
        return;
    }
    if skip_reserved(id, host, report) {
        return;
    }
    let ips = match smtp.queue.source_ip {
        Some(ip) => vec![ip],
        None => match query(t, dns.ip(host)).await {
            Ok(ips) if !ips.is_empty() => ips,
            Ok(_) => {
                report.push(
                    id,
                    Status::Warn,
                    host,
                    "у smtp.hostname нет адресов A/AAAA: исходящий адрес для проверки PTR не определён",
                );
                return;
            }
            Err(e) => {
                report.push(id, Status::Fail, host, format!("адреса не получены: {e}"));
                return;
            }
        },
    };
    for ip in ips {
        match query(t, dns.ptr(ip)).await {
            Err(e) => report.push(id, Status::Fail, ip.to_string(), format!("PTR не получен: {e}")),
            Ok(names) if names.iter().any(|n| same_host(n, host)) => report.push(
                id,
                Status::Ok,
                ip.to_string(),
                format!("PTR совпадает с smtp.hostname ({host})"),
            ),
            Ok(names) if names.is_empty() => report.push(
                id,
                Status::Warn,
                ip.to_string(),
                format!("нет PTR: получатели отклоняют почту с адресов без обратной записи (ожидается {host})"),
            ),
            Ok(names) => report.push(
                id,
                Status::Warn,
                ip.to_string(),
                format!("PTR {} не совпадает с smtp.hostname ({host})", names.join(", ")),
            ),
        }
    }
}

/// Службы RFC 6764 §3 (только TLS): по SRV и TXT `path=` клиенты (iOS, macOS, DAVx5, Thunderbird) находят
/// сервер календарей и контактов по адресу почты.
const DAV_SERVICES: [(&str, &str); 2] = [("caldavs", "календарей"), ("carddavs", "контактов")];

/// Домены проверок DAV: основной и домены подписи DKIM (`smtp.dkim.domains`), без повторов.
fn dav_domains(domain: &str, smtp: &SmtpConfig) -> Vec<String> {
    let mut out = vec![domain.to_owned()];
    for d in smtp.dkim.domains.keys() {
        let d = d.trim_end_matches('.').to_ascii_lowercase();
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// Путь из записей TXT `path=` (RFC 6764 §4).
fn txt_path(records: &[String]) -> Option<&str> {
    records.iter().find_map(|r| {
        r.split_whitespace()
            .find_map(|t| t.strip_prefix("path="))
            .map(|p| p.trim_matches('"'))
    })
}

/// SRV `_caldavs._tcp` / `_carddavs._tcp` и TXT `path=` каждого домена при роли dav. Записи нет — `WARN` с готовыми
/// строками зоны: узел — `jmap.autoconfig.host` или `smtp.hostname`, порт — слушателя DAV (`http.dav`, иначе
/// `http.jmap`; не задан — 443), путь — `/dav/`.
async fn dav_srv(
    cfg: &Config,
    domain: &str,
    smtp: &SmtpConfig,
    dns: &dyn Dns,
    t: Duration,
    report: &mut Report,
) {
    let roles = cfg.roles();
    if !roles.contains(tp_contracts::role::Role::Dav) {
        return;
    }
    let http = cfg
        .section::<sections::HttpSection>(sections::HTTP)
        .map(|h| (*h).clone())
        .unwrap_or_default();
    let port = http.dav_listener(roles).0.map_or(443, |l| l.listen.port());
    let auto_host = cfg
        .section::<tp_jmap::JmapConfig>(sections::JMAP)
        .ok()
        .and_then(|j| j.autoconfig.host.clone());
    let host = auto_host
        .unwrap_or_else(|| smtp.hostname.clone())
        .trim_end_matches('.')
        .to_owned();
    let path = tp_dav::PREFIX;
    for d in dav_domains(domain, smtp) {
        for (service, what) in DAV_SERVICES {
            let id = format!("dns.{service}.{d}");
            let name = format!("_{service}._tcp.{d}");
            if skip_reserved(&id, &name, report) {
                continue;
            }
            let srv_line = format!("{name}. SRV 0 1 {port} {host}.");
            let txt_line = format!("{name}. TXT \"path={path}\"");
            let records = match query(t, dns.srv(&name)).await {
                Ok(records) => records,
                Err(e) => {
                    report.push(&id, Status::Fail, name, format!("SRV не получены: {e}"));
                    continue;
                }
            };
            if records.is_empty() {
                report.push(
                    &id,
                    Status::Warn,
                    name,
                    format!(
                        "нет записи SRV: клиенты не найдут сервер {what} по адресу почты (RFC 6764); добавьте в DNS: {srv_line} и {txt_line}"
                    ),
                );
                continue;
            }
            if records.iter().all(|r| r.target.is_empty()) {
                report.push(
                    &id,
                    Status::Warn,
                    name,
                    format!(
                        "SRV с узлом «.»: служба {what} объявлена недоступной (RFC 2782); замените на: {srv_line}"
                    ),
                );
                continue;
            }
            let value = records
                .iter()
                .map(|r| format!("{} {} {} {}", r.priority, r.weight, r.port, r.target))
                .collect::<Vec<_>>()
                .join(", ");
            let well_known = service.trim_end_matches('s');
            match query(t, dns.txt(&name)).await {
                Err(e) => report.push(&id, Status::Fail, name, format!("TXT не получены: {e}")),
                Ok(list) => match txt_path(&list) {
                    Some(p) if p.trim_end_matches('/') == path.trim_end_matches('/') => report.push(
                        &id,
                        Status::Ok,
                        value,
                        format!("SRV и TXT path={p} опубликованы"),
                    ),
                    Some(p) => report.push(
                        &id,
                        Status::Warn,
                        name,
                        format!(
                            "TXT path={p}, а служба {what} на узле — по {path}: исправьте запись на {txt_line}"
                        ),
                    ),
                    None => report.push(
                        &id,
                        Status::Warn,
                        name,
                        format!(
                            "нет TXT path=: клиенты будут искать /.well-known/{well_known} (RFC 6764 §5); добавьте в DNS: {txt_line}"
                        ),
                    ),
                },
            }
        }
    }
}

/// Срок и имена первого сертификата PEM-цепочки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    /// Окончание срока действия.
    pub not_after: SystemTime,
    /// Имена DNS (subjectAltName).
    pub names: Vec<String>,
}

/// Разбор первого сертификата PEM (`x509-parser`).
pub fn cert_info(pem: &[u8]) -> Result<CertInfo, String> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(pem).map_err(|e| e.to_string())?;
    let cert = pem.parse_x509().map_err(|e| e.to_string())?;
    let secs = cert.validity().not_after.timestamp();
    let not_after = UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap_or(0));
    let names = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|san| {
            san.value
                .general_names
                .iter()
                .filter_map(|n| match n {
                    x509_parser::extensions::GeneralName::DNSName(d) => Some((*d).to_owned()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(CertInfo { not_after, names })
}

/// Итог по сроку: истёк — `Fail`, осталось меньше `warn_before` — `Warn`.
pub fn expiry_status(not_after: SystemTime, now: SystemTime, warn_before: Duration) -> Status {
    match not_after.duration_since(now) {
        Err(_) => Status::Fail,
        Ok(left) if left < warn_before => Status::Warn,
        Ok(_) => Status::Ok,
    }
}

/// Имя покрывается сертификатом (точное или `*.` — одна метка).
fn covers(names: &[String], host: &str) -> bool {
    names.iter().any(|n| {
        same_host(n, host)
            || n.strip_prefix("*.").is_some_and(|suffix| {
                host.split_once('.')
                    .is_some_and(|(_, rest)| same_host(rest, suffix))
            })
    })
}

async fn certificate(
    cfg: &Config,
    smtp: &SmtpConfig,
    store: Option<&Arc<dyn SecretStore>>,
    doctor: &DoctorSection,
    now: SystemTime,
    report: &mut Report,
) {
    let id = "tls.certificate";
    let tls = cfg
        .section::<TlsConfig>("tls")
        .map(|t| (*t).clone())
        .unwrap_or_default();
    let acme = cfg
        .section::<AcmeConfig>("acme")
        .map(|a| a.enabled)
        .unwrap_or(false);
    let path = tls.cert_file.display().to_string();
    let file = tls.cert_file.clone();
    let pem = match tokio::task::spawn_blocking(move || std::fs::read(file)).await {
        Ok(Ok(pem)) => pem,
        Ok(Err(e)) => {
            let (status, msg) = if acme {
                (Status::Skipped, "сертификат ещё не выпущен ACME".to_owned())
            } else {
                (Status::Warn, format!("файл не прочитан: {e}"))
            };
            report.push(id, status, path, msg);
            return;
        }
        Err(e) => {
            report.push(id, Status::Fail, path, format!("чтение прервано: {e}"));
            return;
        }
    };
    let info = match cert_info(&pem) {
        Ok(info) => info,
        Err(e) => {
            report.push(
                id,
                Status::Fail,
                path,
                format!("сертификат не разобран: {e}"),
            );
            return;
        }
    };
    let left = info.not_after.duration_since(now).unwrap_or_default();
    let days = left.as_secs() / 86_400;
    let status = expiry_status(info.not_after, now, doctor.cert_warn_before);
    let msg = match status {
        Status::Fail => "срок действия сертификата истёк".to_owned(),
        Status::Warn => format!("сертификат истекает через {days} сут.: продлите"),
        _ => format!("действителен ещё {days} сут."),
    };
    report.push(id, status, path.clone(), msg);
    key(&tls, store, report).await;
    let host = smtp.hostname.trim_end_matches('.');
    if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
        if covers(&info.names, host) {
            report.push(
                "tls.names",
                Status::Ok,
                info.names.join(", "),
                format!("сертификат выдан на {host}"),
            );
        } else {
            report.push(
                "tls.names",
                Status::Warn,
                info.names.join(", "),
                format!("сертификат не покрывает smtp.hostname ({host}): клиенты не проверят TLS"),
            );
        }
    }
}

/// Закрытый ключ секции `tls` подходит к цепочке `cert_file` — та же загрузка, что при запуске узла. При `tls.key`
/// ключ читается из хранилища секретов, файл `key_file` не проверяется; иначе — файл `key_file`. В отчёте — путь
/// или ссылка на секрет, значение ключа не выводится.
async fn key(tls: &TlsConfig, store: Option<&Arc<dyn SecretStore>>, report: &mut Report) {
    let id = "tls.key";
    let (value, loaded) = match (&tls.key, store) {
        (Some(secret), None) => {
            report.push(
                id,
                Status::Skipped,
                format!("секрет {secret}"),
                "хранилище секретов не настроено",
            );
            return;
        }
        (Some(secret), Some(store)) => (
            format!("секрет {secret}"),
            TlsCerts::load_with_secrets(tls, store.clone()).await,
        ),
        (None, _) => (
            tls.key_file.display().to_string(),
            TlsCerts::load(tls).await,
        ),
    };
    match loaded {
        Ok(_) => report.push(id, Status::Ok, value, "ключ подходит к сертификату"),
        Err(e) => report.push(id, Status::Fail, value, message(&e)),
    }
}

/// Слушатели протоколов узла: (роль, параметры).
fn endpoints(cfg: &Config, smtp: &SmtpConfig) -> Vec<(&'static str, EndpointConfig)> {
    let mut out: Vec<(&'static str, EndpointConfig)> = Vec::new();
    out.extend(smtp.inbound.iter().cloned().map(|e| ("smtp-in", e)));
    out.extend(smtp.submission.iter().cloned().map(|e| ("submission", e)));
    if let Ok(imap) = cfg.section::<tp_imap::ImapConfig>(IMAP) {
        out.extend(imap.listeners.iter().cloned().map(|e| ("imap", e)));
    }
    out
}

/// PROXY protocol v2 на балансировщике: у слушателей с `proxy_protocol` должны быть заданы адреса балансировщиков.
fn proxy(cfg: &Config, smtp: &SmtpConfig, report: &mut Report) {
    let id = "proxy.v2";
    let behind: Vec<(&str, EndpointConfig)> = endpoints(cfg, smtp)
        .into_iter()
        .filter(|(_, e)| e.proxy_protocol)
        .collect();
    if behind.is_empty() {
        report.push(
            id,
            Status::Skipped,
            "",
            "слушателей за балансировщиком (proxy_protocol) нет: адрес клиента — адрес соединения",
        );
        return;
    }
    for (role, e) in behind {
        let value = format!("{role} {}", e.listen);
        if e.trusted_proxies.is_empty() {
            report.push(
                id,
                Status::Warn,
                value,
                "trusted_proxies не задан: заголовок PROXY v2 обязателен для всех соединений, прямые подключения \
                 отклоняются; задайте адреса балансировщиков",
            );
        } else {
            report.push(
                id,
                Status::Ok,
                value,
                format!(
                    "заголовок PROXY v2 принимается от {}",
                    e.trusted_proxies
                        .iter()
                        .map(|c| format!("{}/{}", c.network(), c.prefix()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
    }
}

/// Проверки DNS и сертификатов: резолвер `dns` или системный (`None`).
pub async fn check_with(
    cfg: &Config,
    dns: Option<&dyn Dns>,
    store: Option<&Arc<dyn SecretStore>>,
    doctor: &DoctorSection,
    report: &mut Report,
) {
    if let Some(dns) = dns {
        return check(cfg, dns, store, doctor, report).await;
    }
    match SystemDns::new() {
        Ok(dns) => check(cfg, &dns, store, doctor, report).await,
        Err(e) => report.push("dns.resolver", Status::Fail, "", e.to_string()),
    }
}

/// Подстановка резолвера в тестах.
#[cfg(test)]
#[derive(Debug, Default, Clone)]
pub(crate) struct MockDns {
    pub mx: std::collections::HashMap<String, Vec<(u16, String)>>,
    pub srv: std::collections::HashMap<String, Vec<SrvRecord>>,
    pub txt: std::collections::HashMap<String, Vec<String>>,
    pub ip: std::collections::HashMap<String, Vec<IpAddr>>,
    pub ptr: std::collections::HashMap<IpAddr, Vec<String>>,
    pub fail: Option<String>,
}

#[cfg(test)]
#[async_trait]
impl Dns for MockDns {
    async fn mx(&self, domain: &str) -> Result<Vec<(u16, String)>, String> {
        self.fail.clone().map_or(Ok(()), Err)?;
        Ok(self.mx.get(domain).cloned().unwrap_or_default())
    }

    async fn srv(&self, name: &str) -> Result<Vec<SrvRecord>, String> {
        self.fail.clone().map_or(Ok(()), Err)?;
        Ok(self.srv.get(name).cloned().unwrap_or_default())
    }

    async fn txt(&self, name: &str) -> Result<Vec<String>, String> {
        self.fail.clone().map_or(Ok(()), Err)?;
        Ok(self.txt.get(name).cloned().unwrap_or_default())
    }

    async fn ip(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        self.fail.clone().map_or(Ok(()), Err)?;
        Ok(self.ip.get(host).cloned().unwrap_or_default())
    }

    async fn ptr(&self, ip: IpAddr) -> Result<Vec<String>, String> {
        self.fail.clone().map_or(Ok(()), Err)?;
        Ok(self.ptr.get(&ip).cloned().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sections;

    const CERT: &[u8] = include_bytes!("testdata/cert.pem");

    fn config(extra: &str) -> Config {
        config_for("example.org", extra)
    }

    fn config_for(domain: &str, extra: &str) -> Config {
        sections::registry()
            .expect("registry")
            .parse(
                &format!(
                    "domain: {domain}\nprofile: dev\ndatabase:\n  url: postgres://x@y/z?sslmode=disable\n  tls: {{ allow_plaintext: true }}\n\
                     blob:\n  url: file:///tmp/b\n{extra}"
                ),
                Vec::new(),
            )
            .expect("config")
    }

    fn healthy() -> MockDns {
        let ip: IpAddr = "192.0.2.10".parse().expect("ip");
        let mut dns = MockDns::default();
        dns.mx
            .insert("example.org".into(), vec![(10, "mx.example.org".into())]);
        dns.txt.insert(
            "example.org".into(),
            vec!["v=spf1 mx -all".into(), "google-site-verification=x".into()],
        );
        dns.txt.insert(
            "s1._domainkey.example.org".into(),
            vec!["v=DKIM1; k=rsa; p=MIIB".into()],
        );
        dns.txt.insert(
            "_dmarc.example.org".into(),
            vec!["v=DMARC1; p=reject".into()],
        );
        dns.txt.insert(
            "_mta-sts.example.org".into(),
            vec!["v=STSv1; id=2026".into()],
        );
        dns.ip.insert("mx.example.org".into(), vec![ip]);
        dns.ptr.insert(ip, vec!["mx.example.org.".into()]);
        dns
    }

    const SMTP: &str = "smtp:\n  hostname: mx.example.org\n  dkim:\n    domains:\n      example.org:\n        keys:\n          s1: { algorithm: rsa-sha256, key: { secret: \"thepost/dkim#s1\" } }\n";

    async fn run(cfg: &Config, dns: &MockDns) -> Report {
        run_store(cfg, dns, None).await
    }

    async fn run_store(
        cfg: &Config,
        dns: &MockDns,
        store: Option<&Arc<dyn SecretStore>>,
    ) -> Report {
        let mut report = Report::default();
        check(cfg, dns, store, &DoctorSection::default(), &mut report).await;
        report
    }

    fn status(r: &Report, id: &str) -> Option<Status> {
        r.get(id).map(|c| c.status)
    }

    #[tokio::test]
    async fn healthy_domain_passes() {
        let cfg = config(SMTP);
        let r = run(&cfg, &healthy()).await;
        for id in [
            "dns.mx",
            "dns.spf",
            "dns.dkim.example.org",
            "dns.dmarc",
            "dns.mta_sts",
            "dns.ptr",
        ] {
            assert_eq!(status(&r, id), Some(Status::Ok), "{id}: {r:?}");
        }
        assert_eq!(status(&r, "proxy.v2"), Some(Status::Skipped));
    }

    #[tokio::test]
    async fn reserved_zones_skipped() {
        // Эталон `--profile smb`: домен и узел в зоне .test — проверки DNS пропущены, а не провалены.
        let cfg = config_for(
            "example.test",
            "smtp:\n  hostname: mail.example.test\n  dkim:\n    domains:\n      example.test:\n        keys:\n          s1: { algorithm: rsa-sha256, key: { secret: \"thepost/dkim#s1\" } }\n",
        );
        let failing = MockDns {
            fail: Some("SERVFAIL".into()),
            ..MockDns::default()
        };
        let r = run(&cfg, &failing).await;
        for id in [
            "dns.mx",
            "dns.spf",
            "dns.dkim.example.test",
            "dns.dmarc",
            "dns.mta_sts",
            "dns.ptr",
        ] {
            assert_eq!(status(&r, id), Some(Status::Skipped), "{id}: {r:?}");
        }
        let mx = r.get("dns.mx").expect("mx");
        assert!(mx.message.contains("RFC 6761"), "{mx:?}");

        for (name, zone) in [
            ("a.EXAMPLE.", Some("example")),
            ("x.invalid", Some("invalid")),
            ("app.localhost", Some("localhost")),
            ("example.org", None),
            ("testing.org", None),
            ("", None),
        ] {
            assert_eq!(reserved_zone(name), zone, "{name}");
        }
    }

    /// Проверяются все ключи домена: неопубликованный второй селектор — провал с его именем.
    #[tokio::test]
    async fn every_dkim_key_checked() {
        let cfg = config(&format!(
            "{SMTP}          s2: {{ algorithm: ed25519-sha256, key: {{ secret: \"thepost/dkim#s2\" }} }}\n"
        ));
        let r = run(&cfg, &healthy()).await;
        let dkim = r.get("dns.dkim.example.org").expect("dkim");
        assert_eq!(dkim.status, Status::Fail, "{dkim:?}");
        assert_eq!(dkim.value, "s2._domainkey.example.org", "{dkim:?}");
    }

    #[tokio::test]
    async fn missing_records_warn_or_fail() {
        let cfg = config(SMTP);
        let mut dns = healthy();
        dns.mx.clear();
        dns.txt.clear();
        dns.ptr.clear();
        let r = run(&cfg, &dns).await;
        assert_eq!(status(&r, "dns.mx"), Some(Status::Fail), "{r:?}");
        assert_eq!(status(&r, "dns.spf"), Some(Status::Warn));
        assert_eq!(status(&r, "dns.dkim.example.org"), Some(Status::Fail));
        assert_eq!(status(&r, "dns.dmarc"), Some(Status::Warn));
        assert_eq!(status(&r, "dns.mta_sts"), Some(Status::Warn));
        assert_eq!(status(&r, "dns.ptr"), Some(Status::Warn));
        assert_eq!(r.exit_code(), 1);
    }

    #[tokio::test]
    async fn mismatches_and_resolver_failure() {
        let cfg =
            config("smtp:\n  hostname: mail.example.org\n  queue:\n    source_ip: 192.0.2.10\n");
        let mut dns = healthy();
        dns.txt
            .insert("example.org".into(), vec!["v=spf1 +all".into()]);
        let r = run(&cfg, &dns).await;
        // MX — другой узел, PTR — другое имя, SPF разрешает всех, DKIM не настроен.
        assert_eq!(status(&r, "dns.mx"), Some(Status::Warn), "{r:?}");
        assert_eq!(status(&r, "dns.ptr"), Some(Status::Warn));
        assert_eq!(status(&r, "dns.spf"), Some(Status::Warn));
        assert_eq!(status(&r, "dns.dkim"), Some(Status::Warn));

        let failing = MockDns {
            fail: Some("SERVFAIL".into()),
            ..MockDns::default()
        };
        let r = run(&cfg, &failing).await;
        assert_eq!(status(&r, "dns.mx"), Some(Status::Fail));
        let mx = r.get("dns.mx").expect("mx");
        assert!(mx.message.contains("SERVFAIL"), "{mx:?}");
    }

    /// APP-60: роль dav — SRV `_caldavs._tcp`, `_carddavs._tcp` и TXT `path=` домена; записи нет — `WARN` с готовой
    /// строкой зоны (порт — слушателя DAV), есть — `OK`.
    #[tokio::test]
    async fn dav_srv_hints() {
        let cfg = config(&format!(
            "{SMTP}http:\n  jmap: {{ listen: 127.0.0.1:443 }}\n  dav: {{ listen: 127.0.0.1:8443 }}\n"
        ));
        let r = run(&cfg, &healthy()).await;
        for service in ["caldavs", "carddavs"] {
            let c = r.get(&format!("dns.{service}.example.org")).expect(service);
            assert_eq!(c.status, Status::Warn, "{c:?}");
            let name = format!("_{service}._tcp.example.org.");
            assert!(
                c.message
                    .contains(&format!("{name} SRV 0 1 8443 mx.example.org.")),
                "{c:?}"
            );
            assert!(
                c.message.contains(&format!("{name} TXT \"path=/dav/\"")),
                "{c:?}"
            );
        }

        let mut dns = healthy();
        for service in ["caldavs", "carddavs"] {
            let name = format!("_{service}._tcp.example.org");
            dns.srv.insert(
                name.clone(),
                vec![SrvRecord {
                    priority: 0,
                    weight: 1,
                    port: 8443,
                    target: "mx.example.org".into(),
                }],
            );
            dns.txt.insert(name, vec!["path=/dav/".into()]);
        }
        let r = run(&cfg, &dns).await;
        for id in ["dns.caldavs.example.org", "dns.carddavs.example.org"] {
            assert_eq!(status(&r, id), Some(Status::Ok), "{id}: {r:?}");
        }

        // SRV есть, TXT нет — предупреждение со строкой TXT.
        dns.txt.remove("_carddavs._tcp.example.org");
        let r = run(&cfg, &dns).await;
        let c = r.get("dns.carddavs.example.org").expect("carddavs");
        assert_eq!(c.status, Status::Warn, "{c:?}");
        assert!(
            c.message
                .contains("_carddavs._tcp.example.org. TXT \"path=/dav/\""),
            "{c:?}"
        );

        // Роль dav выключена — проверок нет.
        let r = run(&config(&format!("{SMTP}roles: [jmap]\n")), &healthy()).await;
        assert!(r.get("dns.caldavs.example.org").is_none(), "{r:?}");
    }

    #[tokio::test]
    async fn certificate_expiry_and_names() {
        let info = cert_info(CERT).expect("cert");
        assert_eq!(info.names, vec!["localhost".to_owned()]);
        let day = Duration::from_secs(86_400);
        let warn = 21 * day;
        assert_eq!(
            expiry_status(info.not_after, info.not_after - 30 * day, warn),
            Status::Ok
        );
        assert_eq!(
            expiry_status(info.not_after, info.not_after - 3 * day, warn),
            Status::Warn
        );
        assert_eq!(
            expiry_status(info.not_after, info.not_after + day, warn),
            Status::Fail
        );
        assert!(covers(&["*.example.org".into()], "mx.example.org"));
        assert!(!covers(&["*.example.org".into()], "a.mx.example.org"));

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cert.pem");
        std::fs::write(&path, CERT).expect("write");
        let cfg = config(&format!(
            "smtp:\n  hostname: mx.example.org\ntls:\n  cert_file: \"{}\"\n  key_file: \"{}\"\n",
            path.display().to_string().replace('\\', "/"),
            path.display().to_string().replace('\\', "/"),
        ));
        let r = run(&cfg, &healthy()).await;
        assert_eq!(status(&r, "tls.certificate"), Some(Status::Ok), "{r:?}");
        assert_eq!(status(&r, "tls.names"), Some(Status::Warn));

        let cfg = config("tls:\n  cert_file: /nonexistent/cert.pem\n");
        let r = run(&cfg, &healthy()).await;
        assert_eq!(status(&r, "tls.certificate"), Some(Status::Warn));
    }

    /// APP-35: ключ в файле — пара проверяется; `tls.key` — файл `key_file` не читается, секрет читается из
    /// хранилища и сверяется с цепочкой; значение ключа в отчёт не попадает.
    #[tokio::test]
    async fn tls_key_from_file_or_secret() {
        const KEY: &str = include_str!("testdata/key.pem");
        const OTHER: &str = include_str!("testdata/key-rsa.pem");
        let dir = tempfile::tempdir().expect("tempdir");
        let cert = dir.path().join("cert.pem");
        let key_file = dir.path().join("key.pem");
        std::fs::write(&cert, CERT).expect("cert");
        std::fs::write(&key_file, KEY).expect("key");
        let path = |p: &std::path::Path| p.display().to_string().replace('\\', "/");
        let dns = healthy();

        let files = config(&format!(
            "tls:\n  cert_file: \"{}\"\n  key_file: \"{}\"\n",
            path(&cert),
            path(&key_file)
        ));
        let r = run(&files, &dns).await;
        assert_eq!(status(&r, "tls.key"), Some(Status::Ok), "{r:?}");
        std::fs::write(&key_file, OTHER).expect("key");
        let r = run(&files, &dns).await;
        assert_eq!(status(&r, "tls.key"), Some(Status::Fail), "{r:?}");

        // Файла key_file нет (значение по умолчанию): при tls.key о нём не сообщается.
        let secret: tp_contracts::secrets::SecretRef = "thepost/tls#key".parse().expect("ref");
        let by_secret = config(&format!(
            "tls:\n  cert_file: \"{}\"\n  key: {{ secret: \"{secret}\" }}\n",
            path(&cert)
        ));
        let mem = Arc::new(tp_testkit::MemSecretStore::default());
        let store: Arc<dyn SecretStore> = mem.clone();
        let r = run_store(&by_secret, &dns, None).await;
        assert_eq!(status(&r, "tls.key"), Some(Status::Skipped), "{r:?}");
        let r = run_store(&by_secret, &dns, Some(&store)).await;
        assert_eq!(status(&r, "tls.key"), Some(Status::Fail), "{r:?}");
        mem.set(&secret, OTHER);
        let r = run_store(&by_secret, &dns, Some(&store)).await;
        assert_eq!(status(&r, "tls.key"), Some(Status::Fail), "{r:?}");
        mem.set(&secret, KEY);
        let r = run_store(&by_secret, &dns, Some(&store)).await;
        let check = r.get("tls.key").expect("tls.key");
        assert_eq!(check.status, Status::Ok, "{r:?}");
        assert_eq!(check.value, "секрет thepost/tls#key");
        let text = format!("{r:?}");
        assert!(!text.contains("privkey.pem"), "{text}");
        assert!(!text.contains("PRIVATE KEY"), "{text}");
    }

    #[tokio::test]
    async fn proxy_v2_listeners() {
        let cfg = config(
            "imap:\n  listeners:\n    - { listen: \"0.0.0.0:993\", implicit_tls: true, proxy_protocol: true, trusted_proxies: [\"10.0.0.0/8\"] }\n\
             smtp:\n  inbound:\n    - { listen: \"0.0.0.0:25\", proxy_protocol: true }\n",
        );
        let r = run(&cfg, &healthy()).await;
        let checks: Vec<_> = r.checks.iter().filter(|c| c.id == "proxy.v2").collect();
        assert_eq!(checks.len(), 2, "{checks:?}");
        assert!(
            checks
                .iter()
                .any(|c| c.status == Status::Warn && c.value.starts_with("smtp-in"))
        );
        assert!(checks.iter().any(|c| c.status == Status::Ok
            && c.value.starts_with("imap")
            && c.message.contains("10.0.0.0/8")));
    }
}
