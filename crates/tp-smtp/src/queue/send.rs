//! Отправка на MX получателя (RFC 5321, 5) клиентом `mail-send`: MX по предпочтению (нет MX — сам домен,
//! «нулевой» MX RFC 7505 — отказ), адреса узла, соединение с `queue.source_ip`, STARTTLS по `queue.tls`,
//! `MAIL`, `RCPT` каждому получателю, `DATA`. Сбой соединения или приветствия — следующий адрес или MX;
//! все исчерпаны — временная неудача. `queue.relay` — все письма через узел-ретранслятор.
//!
//! Исходящий адрес `queue.source_ip` задаёт семейство адресов узлов: с IPv4-адреса пробуются только A-записи,
//! с IPv6 — только AAAA; узел без адресов этого семейства — временная неудача, а не «домен не найден».
//! Оппортунистический TLS (RFC 7435): сбой STARTTLS или рукопожатия — повтор на том же адресе без TLS,
//! как уровень `may` Postfix; с `queue.tls: require` — сбой узла.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use mail_auth::{DnsError, IpLookupStrategy, MessageAuthenticator};
use mail_send::SmtpClient;
use smtp_proto::{EXT_SIZE, EXT_SMTP_UTF8, EXT_START_TLS, Response};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls;

use super::{RcptResult, SendRequest, Transport};
use crate::authcheck::DnsCache;
use crate::config::OutboundTls;

/// Отправка через DNS (MX) или ретранслятор.
pub struct MxTransport {
    resolver: MessageAuthenticator,
    cache: DnsCache,
    verified: TlsConnector,
    opportunistic: TlsConnector,
}

impl std::fmt::Debug for MxTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MxTransport").finish_non_exhaustive()
    }
}

/// Неудача соединения с узлом: пробуется следующий.
struct HostError {
    reason: String,
    /// Сбой на шаге STARTTLS (команда или рукопожатие): при оппортунистическом TLS — повтор без TLS.
    tls: bool,
}

impl HostError {
    fn new(reason: String) -> Self {
        Self { reason, tls: false }
    }
}

impl MxTransport {
    /// Отправка с резолвером `resolver` (системный — `MessageAuthenticator::new_system_conf`) и кэшем
    /// до `cache_size` записей DNS каждого типа.
    pub fn new(resolver: MessageAuthenticator, cache_size: usize) -> Result<Self, String> {
        // Провайдер rustls процесса (в сборке включены и ring, и aws-lc-rs — выбирается явно).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        Ok(Self {
            resolver,
            cache: DnsCache::new(cache_size),
            verified: mail_send::smtp::tls::build_tls_connector(false)?,
            opportunistic: mail_send::smtp::tls::build_tls_connector(true)?,
        })
    }

    /// Кэш DNS (тесты заполняют его записями вместо сети).
    pub fn cache(&self) -> &DnsCache {
        &self.cache
    }

    /// Узлы для домена: `(имя, порт)` по предпочтению MX. `Err` — итог для всех получателей.
    async fn hosts(&self, req: &SendRequest<'_>) -> Result<Vec<(String, u16)>, RcptResult> {
        if let Some(relay) = &req.cfg.relay {
            let (host, port) = match relay.rsplit_once(':') {
                Some((h, p)) => (h, p.parse().unwrap_or(25)),
                None => (relay.as_str(), 25),
            };
            return Ok(vec![(host.trim_matches(['[', ']']).to_owned(), port)]);
        }
        let port = req.cfg.port;
        match self
            .resolver
            .mx_lookup(req.domain, Some(&self.cache.mx))
            .await
        {
            Ok(set) => {
                let mut hosts = Vec::new();
                for mx in set.rrset.iter() {
                    for ex in mx.exchanges.iter() {
                        let name = ex.trim_end_matches('.');
                        if name.is_empty() {
                            // RFC 7505: домен не принимает почту.
                            return Err(RcptResult::Permanent(
                                "556 5.1.10 recipient domain does not accept mail (null MX)".into(),
                            ));
                        }
                        hosts.push((name.to_owned(), port));
                    }
                }
                if hosts.is_empty() {
                    hosts.push((req.domain.to_owned(), port));
                }
                hosts.truncate(req.cfg.max_mx.max(1));
                Ok(hosts)
            }
            // Нет MX — сам домен (RFC 5321, 5.1).
            Err(mail_auth::Error::Dns(DnsError::RecordNotFound(_))) => {
                Ok(vec![(req.domain.to_owned(), port)])
            }
            Err(e) => Err(RcptResult::Temporary(format!("DNS MX {}: {e}", req.domain))),
        }
    }

    /// Адреса узла `host` семейства исходящего адреса `source` (`None` — IPv4, затем IPv6).
    async fn addresses(
        &self,
        host: &str,
        source: Option<IpAddr>,
    ) -> Result<Vec<IpAddr>, mail_auth::Error> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }
        let strategy = match source {
            None => IpLookupStrategy::Ipv4thenIpv6,
            Some(IpAddr::V4(_)) => IpLookupStrategy::Ipv4Only,
            Some(IpAddr::V6(_)) => IpLookupStrategy::Ipv6Only,
        };
        self.resolver
            .ip_lookup(
                host,
                strategy,
                4,
                Some(&self.cache.ipv4),
                Some(&self.cache.ipv6),
            )
            .await
    }

    /// У узла есть адреса другого семейства, чем исходящий адрес `source`.
    async fn other_family(&self, host: &str, source: IpAddr) -> bool {
        let other = match source {
            IpAddr::V4(_) => IpLookupStrategy::Ipv6Only,
            IpAddr::V6(_) => IpLookupStrategy::Ipv4Only,
        };
        self.resolver
            .ip_lookup(
                host,
                other,
                1,
                Some(&self.cache.ipv4),
                Some(&self.cache.ipv6),
            )
            .await
            .is_ok_and(|ips| !ips.is_empty())
    }

    /// Одна попытка на адрес `addr` узла `host`; `plain` — без STARTTLS (повтор после сбоя TLS).
    async fn attempt(
        &self,
        req: &SendRequest<'_>,
        host: &str,
        addr: SocketAddr,
        plain: bool,
    ) -> Result<Vec<RcptResult>, HostError> {
        let timeout = req.cfg.timeout;
        let err = |e: mail_send::Error| HostError::new(format!("{host} [{}]: {e}", addr.ip()));
        let mut client = match req.cfg.source_ip {
            Some(local) => SmtpClient::connect_using(local, addr, timeout).await,
            None => SmtpClient::connect(addr, timeout).await,
        }
        .map_err(err)?;
        let greeting = tokio::time::timeout(timeout, client.read())
            .await
            .map_err(|_| err(mail_send::Error::Timeout))?
            .map_err(err)?;
        if greeting.code / 100 != 2 {
            return Err(HostError::new(format!(
                "{host}: greeting {}",
                fmt_reply(&greeting)
            )));
        }
        let ehlo = client.ehlo(req.helo).await.map_err(err)?;
        if ehlo.has_capability(EXT_START_TLS) && !plain {
            let connector = match req.cfg.tls {
                OutboundTls::Require => &self.verified,
                OutboundTls::Opportunistic => &self.opportunistic,
            };
            let mut tls = client
                .start_tls(connector, host)
                .await
                .map_err(|e| HostError {
                    tls: true,
                    ..err(e)
                })?;
            let ehlo = tls.ehlo(req.helo).await.map_err(err)?;
            let out = transaction(&mut tls, req, ehlo.capabilities).await;
            let _ = tls.quit().await;
            out.map_err(err)
        } else if req.cfg.tls == OutboundTls::Require {
            let _ = client.quit().await;
            Err(HostError::new(format!("{host}: STARTTLS not offered")))
        } else {
            let out = transaction(&mut client, req, ehlo.capabilities).await;
            let _ = client.quit().await;
            out.map_err(err)
        }
    }
}

#[async_trait]
impl Transport for MxTransport {
    async fn send(&self, req: SendRequest<'_>) -> Vec<RcptResult> {
        let all = |r: RcptResult| vec![r; req.rcpts.len()];
        let hosts = match self.hosts(&req).await {
            Ok(h) => h,
            Err(r) => return all(r),
        };
        let mut last = String::from("no hosts");
        let mut unknown = 0;
        let source = req.cfg.source_ip;
        for (host, port) in &hosts {
            let ips = match self.addresses(host, source).await {
                Ok(ips) if !ips.is_empty() => ips,
                Ok(_) | Err(mail_auth::Error::Dns(DnsError::RecordNotFound(_))) => {
                    match source {
                        Some(src) if self.other_family(host, src).await => {
                            let family = if src.is_ipv4() { "IPv4" } else { "IPv6" };
                            last = format!("{host}: no {family} address for egress {src}");
                        }
                        _ => {
                            unknown += 1;
                            last = format!("{host}: no address");
                        }
                    }
                    continue;
                }
                Err(e) => {
                    last = format!("{host}: DNS {e}");
                    continue;
                }
            };
            for ip in ips {
                let addr = SocketAddr::new(ip, *port);
                let mut res = self.attempt(&req, host, addr, false).await;
                if let Err(e) = &res
                    && e.tls
                    && req.cfg.tls == OutboundTls::Opportunistic
                {
                    tracing::info!(domain = %req.domain, error = %e.reason, "smtp: сбой TLS, повтор без TLS");
                    res = self.attempt(&req, host, addr, true).await;
                }
                match res {
                    Ok(results) => return results,
                    Err(e) => {
                        tracing::info!(domain = %req.domain, error = %e.reason, "smtp: узел недоступен");
                        last = e.reason;
                    }
                }
            }
        }
        if unknown == hosts.len() && req.cfg.relay.is_none() {
            return all(RcptResult::Permanent(format!(
                "550 5.1.2 recipient domain {} not found",
                req.domain
            )));
        }
        all(RcptResult::Temporary(last))
    }
}

/// `MAIL`, `RCPT`, `DATA` на установленном соединении. Ошибка ввода-вывода — сбой узла.
async fn transaction<T: AsyncRead + AsyncWrite + Unpin>(
    c: &mut SmtpClient<T>,
    req: &SendRequest<'_>,
    caps: u32,
) -> Result<Vec<RcptResult>, mail_send::Error> {
    let mut params = String::new();
    if caps & EXT_SIZE != 0 {
        params.push_str(&format!(" SIZE={}", req.message.len()));
    }
    let utf8 = !req.from.is_ascii() || req.rcpts.iter().any(|r| !r.is_ascii());
    if utf8 {
        if caps & EXT_SMTP_UTF8 == 0 {
            return Ok(vec![
                RcptResult::Permanent(
                    "553 5.6.7 recipient server does not support SMTPUTF8".into()
                );
                req.rcpts.len()
            ]);
        }
        params.push_str(" SMTPUTF8");
    }
    let r = c
        .cmd(format!("MAIL FROM:<{}>{params}\r\n", req.from))
        .await?;
    if r.code / 100 != 2 {
        let res = classify(&r);
        let _ = c.cmd(b"RSET\r\n").await;
        return Ok(vec![res; req.rcpts.len()]);
    }
    let mut out = Vec::with_capacity(req.rcpts.len());
    let mut accepted = Vec::new();
    for (i, rcpt) in req.rcpts.iter().enumerate() {
        let r = c.cmd(format!("RCPT TO:<{rcpt}>\r\n")).await?;
        if r.code / 100 == 2 {
            accepted.push(i);
            out.push(RcptResult::Delivered(String::new()));
        } else {
            out.push(classify(&r));
        }
    }
    if accepted.is_empty() {
        let _ = c.cmd(b"RSET\r\n").await;
        return Ok(out);
    }
    let r = c.cmd(b"DATA\r\n").await?;
    let final_reply = if r.code == 354 {
        tokio::time::timeout(c.timeout, async {
            // `write_message` дописывает `\r\n.\r\n`: завершающий перевод строки письма — его часть.
            let body = req.message.strip_suffix(b"\r\n").unwrap_or(req.message);
            c.write_message(body).await?;
            c.read().await
        })
        .await
        .map_err(|_| mail_send::Error::Timeout)??
    } else {
        r
    };
    let res = classify(&final_reply);
    for i in accepted {
        out[i] = res.clone();
    }
    Ok(out)
}

/// Ответ сервера: 2xx — доставлено, 4xx — повтор, 5xx — отказ.
fn classify(r: &Response<String>) -> RcptResult {
    let text = fmt_reply(r);
    match r.code / 100 {
        2 => RcptResult::Delivered(text),
        5 => RcptResult::Permanent(text),
        _ => RcptResult::Temporary(text),
    }
}

/// `"550 5.1.1 текст"` (одной строкой).
pub fn fmt_reply(r: &Response<String>) -> String {
    let msg = r.message.replace(['\r', '\n'], " ");
    if r.esc[0] == 0 {
        format!("{} {}", r.code, msg.trim())
    } else {
        format!(
            "{} {}.{}.{} {}",
            r.code,
            r.esc[0],
            r.esc[1],
            r.esc[2],
            msg.trim()
        )
    }
}

/// Отправка как `Arc<dyn Transport>`.
pub fn shared(t: MxTransport) -> Arc<dyn Transport> {
    Arc::new(t)
}
