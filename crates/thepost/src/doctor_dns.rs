//! `thepost doctor`: DNS почтового домена, сертификаты и PROXY v2 (SRS 5.5; APP-06).
//!
//! Проверки: MX основного домена (и указывает ли он на `smtp.hostname`), SPF, DKIM по селекторам
//! `smtp.dkim.domains`, DMARC, MTA-STS, PTR исходящих адресов (`smtp.queue.source_ip` или адреса
//! `smtp.hostname`), срок и имена сертификата `tls`, слушатели с PROXY protocol v2 за балансировщиком.
//! DNS — через [`Dns`]: в работе — системный резолвер (`mail-auth`), в тестах — подстановка.

use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use mail_auth::{MX, MessageAuthenticator, RecordSet};
use tp_platform::listen::EndpointConfig;
use tp_platform::{AcmeConfig, Config, TlsConfig};
use tp_smtp::SmtpConfig;
use tp_smtp::authcheck::MemCache;

use crate::doctor::{Report, Status};
use crate::sections::{DoctorSection, IMAP, SMTP};

/// Запросы DNS проверок. Нет записей (NXDOMAIN, пустой ответ) — пустой список; сбой — `Err`.
#[async_trait]
pub trait Dns: Send + Sync {
    /// MX: (предпочтение, узел) в порядке предпочтения.
    async fn mx(&self, domain: &str) -> Result<Vec<(u16, String)>, String>;
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

type NoMxCache = MemCache<Box<str>, RecordSet<MX>>;
type NoPtrCache = MemCache<IpAddr, RecordSet<Box<str>>>;
type NoV4Cache = MemCache<Box<str>, RecordSet<std::net::Ipv4Addr>>;
type NoV6Cache = MemCache<Box<str>, RecordSet<std::net::Ipv6Addr>>;

#[async_trait]
impl Dns for SystemDns {
    async fn mx(&self, domain: &str) -> Result<Vec<(u16, String)>, String> {
        let r = self
            .0
            .mx_lookup(domain, None::<&NoMxCache>)
            .await
            .map(|set| {
                let mut out: Vec<(u16, String)> = set
                    .rrset
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
                    None::<&NoV4Cache>,
                    None::<&NoV6Cache>,
                )
                .await,
        )
    }

    async fn ptr(&self, ip: IpAddr) -> Result<Vec<String>, String> {
        let r = self.0.ptr_lookup(ip, None::<&NoPtrCache>).await.map(|set| {
            set.rrset
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

/// Все проверки DNS, сертификатов и PROXY v2.
pub async fn check(cfg: &Config, dns: &dyn Dns, doctor: &DoctorSection, report: &mut Report) {
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
    certificate(cfg, &smtp, doctor, SystemTime::now(), report).await;
    proxy(cfg, &smtp, report);
}

async fn mx(domain: &str, smtp: &SmtpConfig, dns: &dyn Dns, t: Duration, report: &mut Report) {
    let id = "dns.mx";
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
    for (domain, d) in &smtp.dkim.domains {
        let id = format!("dns.dkim.{domain}");
        let name = format!("{}._domainkey.{domain}", d.selector);
        match query(t, dns.txt(&name)).await {
            Err(e) => report.push(&id, Status::Fail, name, format!("TXT не получены: {e}")),
            Ok(list) if list.iter().any(|r| r.contains("p=")) => {
                report.push(&id, Status::Ok, name, "открытый ключ опубликован")
            }
            Ok(_) => report.push(
                &id,
                Status::Fail,
                name,
                "открытый ключ селектора не опубликован: подписанные письма не пройдут проверку DKIM",
            ),
        }
    }
}

async fn dmarc(domain: &str, dns: &dyn Dns, t: Duration, report: &mut Report) {
    let id = "dns.dmarc";
    let name = format!("_dmarc.{domain}");
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
    doctor: &DoctorSection,
    report: &mut Report,
) {
    if let Some(dns) = dns {
        return check(cfg, dns, doctor, report).await;
    }
    match SystemDns::new() {
        Ok(dns) => check(cfg, &dns, doctor, report).await,
        Err(e) => report.push("dns.resolver", Status::Fail, "", e.to_string()),
    }
}

/// Подстановка резолвера в тестах.
#[cfg(test)]
#[derive(Debug, Default, Clone)]
pub(crate) struct MockDns {
    pub mx: std::collections::HashMap<String, Vec<(u16, String)>>,
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
        sections::registry()
            .expect("registry")
            .parse(
                &format!(
                    "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://x@y/z?sslmode=disable\n  tls: {{ allow_plaintext: true }}\n\
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

    const SMTP: &str = "smtp:\n  hostname: mx.example.org\n  dkim:\n    domains:\n      example.org:\n        selector: s1\n        keys:\n          s1: { algorithm: rsa-sha256, key: { secret: \"thepost/dkim#s1\" } }\n";

    async fn run(cfg: &Config, dns: &MockDns) -> Report {
        let mut report = Report::default();
        check(cfg, dns, &DoctorSection::default(), &mut report).await;
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
