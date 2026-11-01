//! Экспорт журнала аудита во внешнюю SIEM (SRS 5.2, 6.8): syslog и OTLP.
//!
//! - [`SyslogAudit`] — сообщения RFC 5424 (`<PRI>1 TIMESTAMP HOST APP PROCID audit - BOM{json}`, facility по
//!   умолчанию 13 «log audit», severity 5 «notice») кадрами с подсчётом октетов (RFC 5425) поверх TLS (CA из
//!   файла или системные корни, mTLS). Отправка — фоновой задачей через ограниченную очередь: запрос администратора
//!   не ждёт SIEM; при недоступности приёмника задача переподключается, переполнение очереди — потеря записи
//!   с ошибкой в журнале узла (основная копия — в хранилище журнала аудита).
//! - [`OtlpAudit`] — записи OTLP-логов (событие `thepost.admin.audit`, тело — запись JSON, атрибуты —
//!   организация, администратор, метод, маршрут, код) через пакетный экспорт OTLP/HTTP.
//!
//! [`Fanout`] пишет запись во все приёмники (хранилище и экспорт); [`AuditExportConfig::start`] собирает
//! экспорт по секции `admin.audit`.

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::{SdkLogger, SdkLoggerProvider};
use rustls::ClientConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use schemars::JsonSchema;
use serde::Deserialize;
use time::UtcOffset;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;
use tp_contracts::config::duration;
use tp_contracts::tls::{InternalTls, Profile};
use tp_contracts::{Error, Result};

use crate::audit::{AuditRecord, AuditSink};

/// Секция `admin.audit`: экспорт журнала аудита; без подсекций — выключен.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AuditExportConfig {
    /// Syslog (RFC 5424 поверх TLS).
    pub syslog: Option<SyslogConfig>,
    /// OTLP-логи.
    pub otlp: Option<OtlpAuditConfig>,
}

impl AuditExportConfig {
    /// Проверяет подсекции (без профиля; открытый канал syslog проверяет [`SyslogAudit::start`]).
    pub fn check(&self) -> Result<()> {
        if let Some(s) = &self.syslog {
            s.check()?;
        }
        if let Some(o) = &self.otlp
            && o.endpoint.is_empty()
        {
            return Err(Error::invalid("admin.audit.otlp: endpoint is empty"));
        }
        Ok(())
    }

    /// Запускает экспорт по секции: приёмники для [`Fanout`] вместе с хранилищем журнала.
    pub async fn start(&self, profile: Profile) -> Result<Vec<Arc<dyn AuditSink>>> {
        self.check()?;
        let mut out: Vec<Arc<dyn AuditSink>> = Vec::new();
        if let Some(s) = &self.syslog {
            out.push(Arc::new(SyslogAudit::start(s.clone(), profile).await?));
        }
        if let Some(o) = self.otlp.clone() {
            // Клиент OTLP/HTTP блокирующий: создаётся вне async-контекста.
            let sink = tokio::task::spawn_blocking(move || OtlpAudit::http(&o))
                .await
                .map_err(|e| Error::internal(format!("admin.audit.otlp: {e}")))??;
            out.push(Arc::new(sink));
        }
        Ok(out)
    }
}

/// Запись во все приёмники по порядку; ошибка любого — первая ошибка после попытки записать во все.
#[derive(Clone, Default)]
pub struct Fanout(pub Vec<Arc<dyn AuditSink>>);

impl fmt::Debug for Fanout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Fanout").field(&self.0.len()).finish()
    }
}

#[async_trait]
impl AuditSink for Fanout {
    async fn record(&self, record: &AuditRecord) -> Result<()> {
        let mut first = None;
        for s in &self.0 {
            if let Err(e) = s.record(record).await {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }
}

// ---------- syslog ----------

/// Подсекция `admin.audit.syslog`.
///
/// ```yaml
/// address: siem.example.org:6514
/// tls: { ca_file: /etc/thepost/siem-ca.pem }
/// hostname: mail1.example.org
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SyslogConfig {
    /// Приёмник `узел:порт` (syslog поверх TLS, обычно 6514).
    pub address: String,
    /// TLS: CA приёмника (не задан — системные корни), сертификат клиента, `server_name`; `allow_plaintext` —
    /// открытый TCP, только профиль dev.
    pub tls: InternalTls,
    /// Поле HOSTNAME; не задано — `-`.
    pub hostname: Option<String>,
    /// Поле APP-NAME.
    pub app_name: String,
    /// Facility (0–23); 13 — «log audit».
    pub facility: u8,
    /// Наибольшее число записей в очереди отправки.
    pub queue: usize,
    /// Таймаут подключения и рукопожатия TLS.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub connect_timeout: Duration,
    /// Пауза перед повторным подключением.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry: Duration,
}

impl Default for SyslogConfig {
    fn default() -> Self {
        Self {
            address: String::new(),
            tls: InternalTls::default(),
            hostname: None,
            app_name: "thepost".to_owned(),
            facility: 13,
            queue: 10_000,
            connect_timeout: Duration::from_secs(5),
            retry: Duration::from_secs(5),
        }
    }
}

/// Поле заголовка RFC 5424: печатные ASCII без пробела, 1..=`max` символов.
fn header_field(what: &str, v: &str, max: usize) -> Result<()> {
    if v.is_empty() || v.len() > max || !v.bytes().all(|b| (33..=126).contains(&b)) {
        return Err(Error::invalid(format!(
            "admin.audit.syslog: {what} — 1..={max} printable ASCII characters without spaces"
        )));
    }
    Ok(())
}

impl SyslogConfig {
    /// Проверяет подсекцию: адрес `узел:порт`, facility, поля заголовка, очередь, пара сертификат и ключ.
    pub fn check(&self) -> Result<()> {
        if host_of(&self.address).is_none() {
            return Err(Error::invalid(
                "admin.audit.syslog: address must be host:port",
            ));
        }
        if self.facility > 23 {
            return Err(Error::invalid("admin.audit.syslog: facility is 0..=23"));
        }
        header_field("app_name", &self.app_name, 48)?;
        if let Some(h) = &self.hostname {
            header_field("hostname", h, 255)?;
        }
        if self.queue == 0 {
            return Err(Error::invalid("admin.audit.syslog: queue must be positive"));
        }
        if self.tls.cert_file.is_some() != self.tls.key_file.is_some() {
            return Err(Error::invalid(
                "admin.audit.syslog: tls.cert_file and tls.key_file must be set together",
            ));
        }
        Ok(())
    }
}

/// Узел из `узел:порт` (`[v6]:порт`); порт — число.
fn host_of(address: &str) -> Option<&str> {
    let (host, port) = address.rsplit_once(':')?;
    port.parse::<u16>().ok()?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    (!host.is_empty()).then_some(host)
}

/// Сообщение RFC 5424 для записи: severity 5 (notice), MSGID `audit`, без структурированных данных,
/// MSG — запись JSON в UTF-8 с BOM.
pub fn rfc5424(r: &AuditRecord, facility: u8, hostname: Option<&str>, app_name: &str) -> String {
    let pri = u16::from(facility) * 8 + 5;
    let t = r.at.to_offset(UtcOffset::UTC);
    let json = serde_json::to_string(r).unwrap_or_default();
    format!(
        "<{pri}>1 {:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z {} {app_name} {} audit - \u{feff}{json}",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.microsecond(),
        hostname.unwrap_or("-"),
        std::process::id(),
    )
}

/// Кадр RFC 5425: длина сообщения в октетах, пробел, сообщение.
fn frame(msg: &str) -> Vec<u8> {
    format!("{} {msg}", msg.len()).into_bytes()
}

/// Экспорт журнала аудита в syslog (RFC 5424 поверх TLS, RFC 5425) фоновой задачей.
#[derive(Debug, Clone)]
pub struct SyslogAudit {
    tx: mpsc::Sender<Vec<u8>>,
    cfg: Arc<SyslogConfig>,
}

type Conn = Box<dyn AsyncWrite + Send + Unpin>;

impl SyslogAudit {
    /// Проверяет подсекцию (открытый канал — только профиль dev), готовит TLS и запускает задачу отправки
    /// (нужен runtime tokio). Подключение — при первой записи.
    pub async fn start(cfg: SyslogConfig, profile: Profile) -> Result<Self> {
        cfg.check()?;
        let tls = if cfg.tls.allow_plaintext {
            if profile != Profile::Dev {
                return Err(Error::invalid(
                    "admin.audit.syslog: tls.allow_plaintext is allowed only with profile: dev",
                ));
            }
            tracing::warn!(
                "audit syslog channel is NOT encrypted (tls.allow_plaintext, profile dev)"
            );
            None
        } else {
            let name = cfg
                .tls
                .server_name
                .clone()
                .or_else(|| host_of(&cfg.address).map(str::to_owned))
                .unwrap_or_default();
            let name = ServerName::try_from(name)
                .map_err(|e| Error::invalid(format!("admin.audit.syslog: server name: {e}")))?;
            let t = cfg.tls.clone();
            let config = tokio::task::spawn_blocking(move || client_tls(&t))
                .await
                .map_err(|e| Error::internal(format!("admin.audit.syslog: tls: {e}")))??;
            Some((TlsConnector::from(Arc::new(config)), name))
        };
        let cfg = Arc::new(cfg);
        let (tx, rx) = mpsc::channel(cfg.queue);
        tokio::spawn(writer(rx, cfg.clone(), tls));
        Ok(Self { tx, cfg })
    }
}

#[async_trait]
impl AuditSink for SyslogAudit {
    async fn record(&self, r: &AuditRecord) -> Result<()> {
        let msg = rfc5424(
            r,
            self.cfg.facility,
            self.cfg.hostname.as_deref(),
            &self.cfg.app_name,
        );
        if let Err(e) = self.tx.try_send(frame(&msg)) {
            tracing::error!(error = %e, route = %r.route, "audit syslog export: record dropped");
        }
        Ok(())
    }
}

/// Отправка кадров по порядку; ошибка соединения — переподключение через `retry` с повтором кадра.
async fn writer(
    mut rx: mpsc::Receiver<Vec<u8>>,
    cfg: Arc<SyslogConfig>,
    tls: Option<(TlsConnector, ServerName<'static>)>,
) {
    let mut conn: Option<Conn> = None;
    while let Some(frame) = rx.recv().await {
        loop {
            if conn.is_none() {
                match connect(&cfg, tls.as_ref()).await {
                    Ok(c) => conn = Some(c),
                    Err(e) => {
                        tracing::warn!(error = %e, "audit syslog: connect failed");
                        tokio::time::sleep(cfg.retry).await;
                        continue;
                    }
                }
            }
            let Some(c) = conn.as_mut() else {
                continue;
            };
            match async {
                c.write_all(&frame).await?;
                c.flush().await
            }
            .await
            {
                Ok(()) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "audit syslog: write failed, reconnecting");
                    conn = None;
                }
            }
        }
    }
}

async fn connect(
    cfg: &SyslogConfig,
    tls: Option<&(TlsConnector, ServerName<'static>)>,
) -> io::Result<Conn> {
    let timed_out = || io::Error::new(io::ErrorKind::TimedOut, "timeout");
    let tcp = tokio::time::timeout(cfg.connect_timeout, TcpStream::connect(&cfg.address))
        .await
        .map_err(|_| timed_out())??;
    match tls {
        None => Ok(Box::new(tcp)),
        Some((connector, name)) => {
            let s = tokio::time::timeout(cfg.connect_timeout, connector.connect(name.clone(), tcp))
                .await
                .map_err(|_| timed_out())??;
            Ok(Box::new(s))
        }
    }
}

/// Конфигурация rustls: CA из `tls.ca_file` или системные корни, сертификат клиента для mTLS. Блокирующий I/O.
fn client_tls(tls: &InternalTls) -> Result<ClientConfig> {
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let mut roots = rustls::RootCertStore::empty();
    match &tls.ca_file {
        Some(path) => {
            let (added, _) = roots.add_parsable_certificates(read_certs(path)?);
            if added == 0 {
                return Err(Error::Invalid(format!(
                    "admin.audit.syslog: tls.ca_file {}: no usable CA certificates",
                    path.display()
                )));
            }
        }
        None => {
            roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        }
    }
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Invalid(format!("admin.audit.syslog: tls: {e}")))?
        .with_root_certificates(roots);
    match (&tls.cert_file, &tls.key_file) {
        (Some(cert), Some(key)) => {
            let key = PrivateKeyDer::from_pem_file(key)
                .map_err(|e| Error::Invalid(format!("admin.audit.syslog: tls.key_file: {e}")))?;
            builder
                .with_client_auth_cert(read_certs(cert)?, key)
                .map_err(|e| Error::Invalid(format!("admin.audit.syslog: tls.cert_file: {e}")))
        }
        _ => Ok(builder.with_no_client_auth()),
    }
}

fn read_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_file_iter(path)
        .and_then(|it| it.collect::<std::result::Result<Vec<_>, _>>())
        .map_err(|e| Error::Invalid(format!("admin.audit.syslog: {}: {e}", path.display())))
}

// ---------- OTLP ----------

/// Подсекция `admin.audit.otlp`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OtlpAuditConfig {
    /// Адрес приёмника логов OTLP/HTTP (protobuf).
    pub endpoint: String,
    /// Атрибут `service.name`.
    pub service_name: String,
}

impl Default for OtlpAuditConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:4318/v1/logs".to_owned(),
            service_name: "thepost".to_owned(),
        }
    }
}

/// Имя события OTLP записи журнала аудита.
pub const OTLP_EVENT: &str = "thepost.admin.audit";

/// Экспорт журнала аудита записями OTLP-логов.
pub struct OtlpAudit {
    provider: SdkLoggerProvider,
    logger: SdkLogger,
}

impl fmt::Debug for OtlpAudit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtlpAudit").finish_non_exhaustive()
    }
}

impl OtlpAudit {
    /// Пакетный экспорт OTLP/HTTP. Клиент HTTP блокирующий: вызывать вне async-контекста
    /// (`spawn_blocking`, как [`AuditExportConfig::start`]).
    pub fn http(cfg: &OtlpAuditConfig) -> Result<Self> {
        let exporter = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_endpoint(&cfg.endpoint)
            .build()
            .map_err(|e| Error::invalid(format!("admin.audit.otlp: {e}")))?;
        let provider = SdkLoggerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                Resource::builder()
                    .with_service_name(cfg.service_name.clone())
                    .build(),
            )
            .build();
        Ok(Self::with_provider(provider))
    }

    /// Поверх готового поставщика логов (свой экспорт, тесты).
    pub fn with_provider(provider: SdkLoggerProvider) -> Self {
        let logger = provider.logger("thepost.admin");
        Self { provider, logger }
    }

    /// Отправляет накопленное и останавливает экспорт.
    pub fn shutdown(&self) -> Result<()> {
        self.provider
            .shutdown()
            .map_err(|e| Error::internal(format!("admin.audit.otlp: {e}")))
    }
}

#[async_trait]
impl AuditSink for OtlpAudit {
    async fn record(&self, r: &AuditRecord) -> Result<()> {
        let json = serde_json::to_string(r).map_err(|e| Error::internal(e.to_string()))?;
        let mut rec = self.logger.create_log_record();
        rec.set_event_name(OTLP_EVENT);
        rec.set_timestamp(SystemTime::from(r.at));
        rec.set_observed_timestamp(SystemTime::now());
        rec.set_severity_number(Severity::Info);
        rec.set_severity_text("INFO");
        rec.set_body(AnyValue::String(json.into()));
        rec.add_attribute(
            "tenant",
            r.tenant.map(|t| t.to_string()).unwrap_or_default(),
        );
        rec.add_attribute("actor", r.actor.to_string());
        rec.add_attribute("actor_login", r.actor_login.clone());
        rec.add_attribute("method", r.method.clone());
        rec.add_attribute("route", r.route.clone());
        rec.add_attribute("status", i64::from(r.status));
        self.logger.emit(rec);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::logs::{LogBatch, LogExporter};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    use tokio::io::AsyncReadExt;
    use tp_contracts::{AccountId, TenantId};

    use super::*;
    use crate::server::AdminRole;

    fn record(route: &str) -> AuditRecord {
        AuditRecord {
            at: OffsetDateTime::parse("2026-11-01T10:20:30.123456789Z", &Rfc3339).expect("time"),
            tenant: Some(TenantId::DEFAULT),
            actor_tenant: TenantId::DEFAULT,
            actor: AccountId::new(),
            actor_login: "admin@example.org".into(),
            roles: vec![AdminRole::OrganizationAdmin],
            method: "POST".into(),
            route: route.into(),
            path: "/api/v1/orgs/x/domains".into(),
            status: 201,
        }
    }

    #[test]
    fn rfc5424_message() {
        let r = record("/api/v1/orgs/{org}/domains");
        let m = rfc5424(&r, 13, Some("mail1.example.org"), "thepost");
        let head = format!(
            "<109>1 2026-11-01T10:20:30.123456Z mail1.example.org thepost {} audit - \u{feff}",
            std::process::id()
        );
        assert!(m.starts_with(&head), "{m}");
        let json: serde_json::Value = serde_json::from_str(&m[head.len()..]).expect("json body");
        assert_eq!(json["actor_login"], "admin@example.org");
        assert_eq!(json["status"], 201);
        assert!(rfc5424(&r, 4, None, "tp").starts_with("<37>1 "));
        assert!(rfc5424(&r, 4, None, "tp").contains("Z - tp "));
        assert_eq!(frame("abc"), b"3 abc");
        // Длина кадра — в октетах (BOM и кириллица — несколько байт).
        assert_eq!(frame("ж"), "2 ж".as_bytes());
    }

    #[test]
    fn syslog_config_check() {
        let ok = SyslogConfig {
            address: "siem.example.org:6514".into(),
            ..SyslogConfig::default()
        };
        ok.check().expect("ok");
        assert!(
            SyslogConfig {
                address: "[::1]:6514".into(),
                ..SyslogConfig::default()
            }
            .check()
            .is_ok()
        );
        for bad in [
            SyslogConfig::default(),
            SyslogConfig {
                address: "siem.example.org".into(),
                ..ok.clone()
            },
            SyslogConfig {
                facility: 24,
                ..ok.clone()
            },
            SyslogConfig {
                app_name: "the post".into(),
                ..ok.clone()
            },
            SyslogConfig {
                hostname: Some(String::new()),
                ..ok.clone()
            },
            SyslogConfig {
                queue: 0,
                ..ok.clone()
            },
        ] {
            assert!(matches!(bad.check(), Err(Error::Invalid(_))), "{bad:?}");
        }
        let cfg: AuditExportConfig = serde_json::from_value(serde_json::json!({
            "syslog": {"address": "siem:6514", "retry": "1s", "tls": {"server_name": "siem.example.org"}},
            "otlp": {}
        }))
        .expect("parse");
        assert_eq!(
            cfg.syslog.as_ref().map(|s| s.retry),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            cfg.otlp.as_ref().map(|o| o.endpoint.as_str()),
            Some("http://localhost:4318/v1/logs")
        );
        cfg.check().expect("check");
        assert!(
            serde_json::from_value::<AuditExportConfig>(serde_json::json!({"kafka": {}})).is_err()
        );
    }

    /// Открытый канал — только dev; кадры RFC 5425 приходят приёмнику по порядку.
    #[tokio::test]
    async fn syslog_export_frames() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let cfg = SyslogConfig {
            address: listener.local_addr().expect("addr").to_string(),
            tls: InternalTls {
                allow_plaintext: true,
                ..InternalTls::default()
            },
            ..SyslogConfig::default()
        };
        assert!(matches!(
            SyslogAudit::start(cfg.clone(), Profile::Prod).await,
            Err(Error::Invalid(_))
        ));
        let sink = SyslogAudit::start(cfg, Profile::Dev).await.expect("start");
        sink.record(&record("/one")).await.expect("one");
        sink.record(&record("/два")).await.expect("two");
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let routes = loop {
            let n = tokio::time::timeout_at(deadline, sock.read(&mut buf))
                .await
                .expect("timely")
                .expect("read");
            assert!(n > 0, "connection closed");
            got.extend_from_slice(&buf[..n]);
            let mut rest = got.as_slice();
            let mut routes = Vec::new();
            while let Some(sp) = rest.iter().position(|b| *b == b' ') {
                let len: usize = std::str::from_utf8(&rest[..sp])
                    .expect("len")
                    .parse()
                    .expect("len");
                let Some(msg) = rest.get(sp + 1..sp + 1 + len) else {
                    break;
                };
                let msg = std::str::from_utf8(msg).expect("utf-8");
                assert!(msg.starts_with("<109>1 "), "{msg}");
                let json = msg.split_once('\u{feff}').expect("bom").1;
                let v: serde_json::Value = serde_json::from_str(json).expect("json");
                routes.push(v["route"].as_str().expect("route").to_owned());
                rest = &rest[sp + 1 + len..];
            }
            if routes.len() == 2 {
                break routes;
            }
        };
        assert_eq!(routes, ["/one", "/два"]);
    }

    /// Экспорт в память: записи текущего вызова.
    #[derive(Debug, Clone, Default)]
    struct Collect(Arc<Mutex<Vec<Exported>>>);

    /// Событие, тело, атрибуты.
    type Exported = (Option<&'static str>, String, Vec<(String, String)>);

    impl LogExporter for Collect {
        async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
            let mut out = self.0.lock().expect("lock");
            for (rec, _) in batch.iter() {
                let body = match rec.body() {
                    Some(AnyValue::String(s)) => s.to_string(),
                    other => format!("{other:?}"),
                };
                let attrs = rec
                    .attributes_iter()
                    .map(|(k, v)| (k.to_string(), format!("{v:?}")))
                    .collect();
                out.push((rec.event_name(), body, attrs));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn otlp_log_records() {
        let collect = Collect::default();
        let provider = SdkLoggerProvider::builder()
            .with_simple_exporter(collect.clone())
            .build();
        let sink = OtlpAudit::with_provider(provider);
        sink.record(&record("/api/v1/orgs/{org}/domains"))
            .await
            .expect("record");
        sink.shutdown().expect("shutdown");
        let got = collect.0.lock().expect("lock").clone();
        assert_eq!(got.len(), 1);
        let (event, body, attrs) = &got[0];
        assert_eq!(*event, Some(OTLP_EVENT));
        let v: serde_json::Value = serde_json::from_str(body).expect("json");
        assert_eq!(v["route"], "/api/v1/orgs/{org}/domains");
        assert!(
            attrs
                .iter()
                .any(|(k, v)| k == "actor_login" && v.contains("admin@example.org")),
            "{attrs:?}"
        );
        assert!(
            attrs
                .iter()
                .any(|(k, v)| k == "status" && v.contains("201"))
        );
    }

    #[test]
    fn otlp_http_builds_outside_runtime() {
        let sink = OtlpAudit::http(&OtlpAuditConfig::default()).expect("build");
        let _ = sink.shutdown();
    }

    #[tokio::test]
    async fn fanout_writes_all_and_reports_first_error() {
        #[derive(Default)]
        struct Count(Mutex<usize>, bool);
        #[async_trait]
        impl AuditSink for Count {
            async fn record(&self, _: &AuditRecord) -> Result<()> {
                *self.0.lock().expect("lock") += 1;
                if self.1 {
                    Err(Error::UNAVAILABLE)
                } else {
                    Ok(())
                }
            }
        }
        let bad = Arc::new(Count(Mutex::new(0), true));
        let good = Arc::new(Count::default());
        let f = Fanout(vec![bad.clone(), good.clone()]);
        assert_eq!(f.record(&record("/x")).await, Err(Error::UNAVAILABLE));
        assert_eq!(*good.0.lock().expect("lock"), 1);
        assert_eq!(*bad.0.lock().expect("lock"), 1);
        assert_eq!(Fanout::default().record(&record("/x")).await, Ok(()));
    }
}
