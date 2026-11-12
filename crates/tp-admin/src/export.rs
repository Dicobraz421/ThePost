//! Выгрузка журнала аудита из базы во внешнюю SIEM (SRS 5.2, 6.8; решение владельца 12.11: база + выгрузка из неё,
//! APP-59): syslog и OTLP.
//!
//! Журнал аудита пишется только в базу ([`AuditLog`], [`crate::audit`]). Выгрузка ([`export_pass`]) читает записи
//! после отметки доставки приёмника ([`AuditLog::export_mark`]), доставляет их по порядку ([`ExportSink::deliver`])
//! и сдвигает отметку ([`AuditLog::advance_export_mark`]): недоступный приёмник догоняет пропущенное, когда снова
//! доступен, ни одна запись не теряется. Ошибка доставки — отметка на месте, следующий проход повторит те же записи.
//! Выгрузку по кругу ведёт роль maintenance узла (одна на кластер — под лизой).
//!
//! - [`SyslogAudit`] — сообщения RFC 5424 (`<PRI>1 TIMESTAMP HOST APP PROCID audit - BOM{json}`, facility по
//!   умолчанию 13 «log audit», severity 5 «notice») кадрами с подсчётом октетов (RFC 5425) поверх TLS (CA из
//!   файла или системные корни, проверка сертификата приёмника, mTLS: ключ клиента — только из хранилища секретов,
//!   решение 100). Соединение держится между проходами, обрыв — переподключение на следующем.
//! - [`OtlpAudit`] — записи OTLP-логов (событие `thepost.admin.audit`, тело — запись JSON, атрибуты — номер,
//!   организация, администратор, метод, маршрут, код) одним экспортом OTLP/HTTP на пачку.
//!
//! Запись в JSON — [`AuditEntry`] (номер `seq` для отсева повторов на стороне SIEM и запись; момент — RFC 3339).

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use opentelemetry::InstrumentationScope;
use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::{LogBatch, LogExporter, SdkLogger, SdkLoggerProvider};
use rustls::ClientConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use schemars::JsonSchema;
use serde::Deserialize;
use time::UtcOffset;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tp_contracts::admin_audit::{AdminAuditEntry, AuditLog};
use tp_contracts::config::duration;
use tp_contracts::secrets::{Secret, SecretStore};
use tp_contracts::tls::{InternalTls, Profile};
use tp_contracts::{Error, Result};

use crate::audit::{AuditEntry, AuditRecord};

/// Секция `admin.audit`: выгрузка журнала аудита; без подсекций `syslog` и `otlp` — выключена.
///
/// ```yaml
/// interval: 1s
/// retry: 5s
/// batch: 256
/// syslog: { address: "siem.example.org:6514" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AuditExportConfig {
    /// Syslog (RFC 5424 поверх TLS).
    pub syslog: Option<SyslogConfig>,
    /// OTLP-логи.
    pub otlp: Option<OtlpAuditConfig>,
    /// Пауза между проходами, когда новых записей нет.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub interval: Duration,
    /// Пауза перед повтором после ошибки доставки (приёмник недоступен).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry: Duration,
    /// Записей в одной доставке.
    pub batch: usize,
}

impl Default for AuditExportConfig {
    fn default() -> Self {
        Self {
            syslog: None,
            otlp: None,
            interval: Duration::from_secs(1),
            retry: Duration::from_secs(5),
            batch: 256,
        }
    }
}

impl AuditExportConfig {
    /// Есть ли приёмники.
    pub fn enabled(&self) -> bool {
        self.syslog.is_some() || self.otlp.is_some()
    }

    /// Проверяет секцию (без профиля; открытый канал syslog проверяет [`SyslogAudit::start`]).
    pub fn check(&self) -> Result<()> {
        if self.batch == 0 {
            return Err(Error::invalid("admin.audit: batch must be positive"));
        }
        if self.interval.is_zero() || self.retry.is_zero() {
            return Err(Error::invalid(
                "admin.audit: interval and retry must be positive",
            ));
        }
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

    /// Приёмники по секции; ключ клиента syslog (`tls.key`) читается из `secrets`. Без runtime tokio не вызывать.
    pub async fn start(
        &self,
        profile: Profile,
        secrets: &dyn SecretStore,
    ) -> Result<Vec<Arc<dyn ExportSink>>> {
        self.check()?;
        let mut out: Vec<Arc<dyn ExportSink>> = Vec::new();
        if let Some(s) = &self.syslog {
            out.push(Arc::new(
                SyslogAudit::start(s.clone(), profile, secrets).await?,
            ));
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

/// Приёмник выгрузки журнала аудита.
#[async_trait]
pub trait ExportSink: Send + Sync + 'static {
    /// Имя — ключ отметки доставки (`syslog`, `otlp`).
    fn name(&self) -> &'static str;

    /// Доставляет записи по порядку; ошибка — пачка не доставлена (проход повторит её с отметки).
    async fn deliver(&self, entries: &[AdminAuditEntry]) -> Result<()>;
}

/// Один проход выгрузки в `sink`: до `batch` записей после отметки доставки, затем сдвиг отметки. Возвращает число
/// доставленных записей (0 — новых нет). Отметку между чтением и сдвигом сдвинул другой узел — записи доставлены
/// повторно, отметка не трогается (предупреждение в журнал узла).
pub async fn export_pass(log: &dyn AuditLog, sink: &dyn ExportSink, batch: usize) -> Result<usize> {
    let mark = log.export_mark(sink.name()).await?;
    let page = log.export_page(mark, batch).await?;
    let Some(last) = page.last().map(|e| e.seq) else {
        return Ok(0);
    };
    sink.deliver(&page).await?;
    if !log.advance_export_mark(sink.name(), mark, last).await? {
        tracing::warn!(
            sink = sink.name(),
            "журнал аудита: отметку выгрузки сдвинул другой узел"
        );
    }
    Ok(page.len())
}

/// Запись выгрузки в JSON: номер и запись.
fn entry_json(e: &AdminAuditEntry) -> String {
    serde_json::to_string(&AuditEntry {
        seq: e.seq,
        record: AuditRecord::from(&e.record),
    })
    .unwrap_or_default()
}

// ---------- syslog ----------

/// Подсекция `admin.audit.syslog`.
///
/// ```yaml
/// address: siem.example.org:6514
/// tls:
///   ca_file: /etc/thepost/siem-ca.pem
///   cert_file: /etc/thepost/siem-client.pem
///   key: { secret: "thepost/tls/siem#key" }
/// hostname: mail1.example.org
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SyslogConfig {
    /// Приёмник `узел:порт` (syslog поверх TLS, обычно 6514).
    pub address: String,
    /// TLS: CA приёмника (не задан — системные корни), сертификат клиента `cert_file` с ключом `key` (ссылка на
    /// секрет, PEM; `key_file` не принимается — решение 100), `server_name`; `allow_plaintext` — открытый TCP,
    /// только профиль dev.
    pub tls: InternalTls,
    /// Поле HOSTNAME; не задано — `-`.
    pub hostname: Option<String>,
    /// Поле APP-NAME.
    pub app_name: String,
    /// Facility (0–23); 13 — «log audit».
    pub facility: u8,
    /// Таймаут подключения, рукопожатия TLS и записи пачки.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub connect_timeout: Duration,
}

impl Default for SyslogConfig {
    fn default() -> Self {
        Self {
            address: String::new(),
            tls: InternalTls::default(),
            hostname: None,
            app_name: "thepost".to_owned(),
            facility: 13,
            connect_timeout: Duration::from_secs(5),
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
    /// Проверяет подсекцию: адрес `узел:порт`, facility, поля заголовка, таймаут, пара сертификат и ключ (ключ —
    /// только `tls.key`).
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
        if self.connect_timeout.is_zero() {
            return Err(Error::invalid(
                "admin.audit.syslog: connect_timeout must be positive",
            ));
        }
        if self.tls.key_file.is_some() {
            return Err(Error::invalid(
                "admin.audit.syslog: tls.key_file is not supported, use tls.key: { secret: \"<path>#<field>\" }",
            ));
        }
        if self.tls.cert_file.is_some() != self.tls.key.is_some() {
            return Err(Error::invalid(
                "admin.audit.syslog: tls.cert_file and tls.key must be set together",
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
/// MSG — запись JSON ([`AuditEntry`]) в UTF-8 с BOM.
pub fn rfc5424(
    e: &AdminAuditEntry,
    facility: u8,
    hostname: Option<&str>,
    app_name: &str,
) -> String {
    let pri = u16::from(facility) * 8 + 5;
    let t = e.record.at.to_offset(UtcOffset::UTC);
    format!(
        "<{pri}>1 {:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z {} {app_name} {} audit - \u{feff}{}",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.microsecond(),
        hostname.unwrap_or("-"),
        std::process::id(),
        entry_json(e),
    )
}

/// Кадр RFC 5425: длина сообщения в октетах, пробел, сообщение.
fn frame(msg: &str) -> Vec<u8> {
    format!("{} {msg}", msg.len()).into_bytes()
}

type Conn = Box<dyn AsyncWrite + Send + Unpin>;

/// Выгрузка журнала аудита в syslog (RFC 5424 поверх TLS, RFC 5425).
pub struct SyslogAudit {
    cfg: SyslogConfig,
    tls: Option<(TlsConnector, ServerName<'static>)>,
    conn: tokio::sync::Mutex<Option<Conn>>,
}

impl fmt::Debug for SyslogAudit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyslogAudit")
            .field("address", &self.cfg.address)
            .field("tls", &self.tls.is_some())
            .finish_non_exhaustive()
    }
}

impl SyslogAudit {
    /// Проверяет подсекцию (открытый канал — только профиль dev), читает ключ клиента `tls.key` из `secrets` и
    /// готовит TLS. Подключение — при первой доставке.
    pub async fn start(
        cfg: SyslogConfig,
        profile: Profile,
        secrets: &dyn SecretStore,
    ) -> Result<Self> {
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
            let key = match &cfg.tls.key {
                Some(r) => Some(secrets.get(r).await.inspect_err(|e| {
                    tracing::error!(error = %e, secret = %r, "audit syslog: tls.key: secret read failed");
                })?),
                None => None,
            };
            let t = cfg.tls.clone();
            let config = tokio::task::spawn_blocking(move || client_tls(&t, key.as_ref()))
                .await
                .map_err(|e| Error::internal(format!("admin.audit.syslog: tls: {e}")))??;
            Some((TlsConnector::from(Arc::new(config)), name))
        };
        Ok(Self {
            cfg,
            tls,
            conn: tokio::sync::Mutex::new(None),
        })
    }

    /// Подключение к приёмнику (TCP и рукопожатие TLS с проверкой сертификата) — как при доставке; для
    /// `thepost doctor`.
    pub async fn probe(&self) -> io::Result<()> {
        connect(&self.cfg, self.tls.as_ref()).await.map(drop)
    }
}

#[async_trait]
impl ExportSink for SyslogAudit {
    fn name(&self) -> &'static str {
        "syslog"
    }

    async fn deliver(&self, entries: &[AdminAuditEntry]) -> Result<()> {
        let mut bytes = Vec::new();
        for e in entries {
            bytes.extend(frame(&rfc5424(
                e,
                self.cfg.facility,
                self.cfg.hostname.as_deref(),
                &self.cfg.app_name,
            )));
        }
        let mut conn = self.conn.lock().await;
        let result = async {
            if conn.is_none() {
                *conn = Some(connect(&self.cfg, self.tls.as_ref()).await?);
            }
            let Some(c) = conn.as_mut() else {
                return Err(io::Error::other("no connection"));
            };
            let write = async {
                c.write_all(&bytes).await?;
                c.flush().await
            };
            tokio::time::timeout(self.cfg.connect_timeout, write)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "write timeout"))?
        }
        .await;
        result.map_err(|e| {
            *conn = None;
            tracing::warn!(error = %e, address = %self.cfg.address, "audit syslog: delivery failed");
            Error::UNAVAILABLE
        })
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

/// Конфигурация rustls: CA из `tls.ca_file` или системные корни, сертификат клиента `tls.cert_file` с ключом
/// `key` (значение секрета `tls.key`, PEM) для mTLS. Блокирующий I/O.
fn client_tls(tls: &InternalTls, key: Option<&Secret>) -> Result<ClientConfig> {
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
    match (&tls.cert_file, key) {
        (Some(cert), Some(key)) => {
            // Текст ошибки разбора не содержит значения ключа.
            let key = PrivateKeyDer::from_pem_slice(key.expose().as_bytes()).map_err(|e| {
                Error::Invalid(format!(
                    "admin.audit.syslog: tls.key: not a PEM private key: {e}"
                ))
            })?;
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

/// Выгрузка журнала аудита записями OTLP-логов: пачка — один вызов экспорта `E`, его ошибка — пачка не доставлена.
pub struct OtlpAudit<E = opentelemetry_otlp::LogExporter> {
    exporter: Arc<E>,
    /// Поставщик без обработчиков: только создание записей.
    _provider: SdkLoggerProvider,
    logger: SdkLogger,
    scope: InstrumentationScope,
}

impl<E> fmt::Debug for OtlpAudit<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtlpAudit").finish_non_exhaustive()
    }
}

impl OtlpAudit {
    /// Экспорт OTLP/HTTP. Клиент HTTP блокирующий: вызывать вне async-контекста (`spawn_blocking`, как
    /// [`AuditExportConfig::start`]).
    pub fn http(cfg: &OtlpAuditConfig) -> Result<Self> {
        let exporter = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_endpoint(&cfg.endpoint)
            .build()
            .map_err(|e| Error::invalid(format!("admin.audit.otlp: {e}")))?;
        Ok(Self::with_exporter(exporter, &cfg.service_name))
    }
}

impl<E: LogExporter + 'static> OtlpAudit<E> {
    /// Поверх своего экспорта (тесты); `service_name` — атрибут ресурса `service.name`.
    pub fn with_exporter(mut exporter: E, service_name: &str) -> Self {
        let resource = Resource::builder()
            .with_service_name(service_name.to_owned())
            .build();
        exporter.set_resource(&resource);
        let provider = SdkLoggerProvider::builder().with_resource(resource).build();
        let logger = provider.logger("thepost.admin");
        Self {
            exporter: Arc::new(exporter),
            _provider: provider,
            logger,
            scope: InstrumentationScope::builder("thepost.admin").build(),
        }
    }
}

#[async_trait]
impl<E: LogExporter + 'static> ExportSink for OtlpAudit<E> {
    fn name(&self) -> &'static str {
        "otlp"
    }

    async fn deliver(&self, entries: &[AdminAuditEntry]) -> Result<()> {
        let records: Vec<_> = entries
            .iter()
            .map(|e| {
                let r = &e.record;
                let mut rec = self.logger.create_log_record();
                rec.set_event_name(OTLP_EVENT);
                rec.set_timestamp(SystemTime::from(r.at));
                rec.set_observed_timestamp(SystemTime::now());
                rec.set_severity_number(Severity::Info);
                rec.set_severity_text("INFO");
                rec.set_body(AnyValue::String(entry_json(e).into()));
                rec.add_attribute("seq", i64::try_from(e.seq).unwrap_or(i64::MAX));
                rec.add_attribute(
                    "tenant",
                    r.tenant.map(|t| t.to_string()).unwrap_or_default(),
                );
                rec.add_attribute("actor", r.actor.to_string());
                rec.add_attribute("actor_login", r.actor_login.clone());
                rec.add_attribute("method", r.method.clone());
                rec.add_attribute("route", r.route.clone());
                rec.add_attribute("status", i64::from(r.status));
                rec
            })
            .collect();
        let (exporter, scope) = (self.exporter.clone(), self.scope.clone());
        let runtime = tokio::runtime::Handle::current();
        // Клиент OTLP/HTTP блокирующий: экспорт — вне async-контекста.
        tokio::task::spawn_blocking(move || {
            let data: Vec<_> = records.iter().map(|r| (r, &scope)).collect();
            runtime.block_on(exporter.export(LogBatch::new(&data)))
        })
        .await
        .map_err(|e| Error::internal(format!("admin.audit.otlp: {e}")))?
        .map_err(|e| {
            tracing::warn!(error = %e, "audit otlp: delivery failed");
            Error::UNAVAILABLE
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    use tokio::io::AsyncReadExt;
    use tp_contracts::admin_audit::AdminAuditRecord;
    use tp_contracts::secrets::SecretRef;
    use tp_contracts::{AccountId, TenantId};
    use tp_testkit::{MemAuditLog, MemSecretStore};

    use super::*;
    use crate::server::AdminRole;

    fn record(route: &str) -> AdminAuditRecord {
        AdminAuditRecord {
            at: OffsetDateTime::parse("2026-11-01T10:20:30.123456789Z", &Rfc3339).expect("time"),
            tenant: Some(TenantId::DEFAULT),
            actor_tenant: TenantId::DEFAULT,
            actor: AccountId::new(),
            actor_login: "admin@example.org".into(),
            roles: vec![AdminRole::OrganizationAdmin.as_str().into()],
            method: "POST".into(),
            route: route.into(),
            path: "/api/v1/orgs/x/transport/domains".into(),
            status: 201,
            content_messages: None,
        }
    }

    fn entry(seq: u64, route: &str) -> AdminAuditEntry {
        AdminAuditEntry {
            seq,
            record: record(route),
        }
    }

    #[test]
    fn rfc5424_message() {
        let e = entry(7, "/api/v1/orgs/{org}/transport/domains");
        let m = rfc5424(&e, 13, Some("mail1.example.org"), "thepost");
        let head = format!(
            "<109>1 2026-11-01T10:20:30.123456Z mail1.example.org thepost {} audit - \u{feff}",
            std::process::id()
        );
        assert!(m.starts_with(&head), "{m}");
        let json: serde_json::Value = serde_json::from_str(&m[head.len()..]).expect("json body");
        assert_eq!(json["seq"], 7);
        assert_eq!(json["actor_login"], "admin@example.org");
        assert_eq!(json["status"], 201);
        assert!(rfc5424(&e, 4, None, "tp").starts_with("<37>1 "));
        assert!(rfc5424(&e, 4, None, "tp").contains("Z - tp "));
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
                connect_timeout: Duration::ZERO,
                ..ok.clone()
            },
            // Ключ только из хранилища секретов (решение 100); сертификат и ключ — парой.
            SyslogConfig {
                tls: InternalTls {
                    cert_file: Some("/etc/thepost/siem-client.pem".into()),
                    key_file: Some("/etc/thepost/siem-client.key".into()),
                    ..InternalTls::default()
                },
                ..ok.clone()
            },
            SyslogConfig {
                tls: InternalTls {
                    cert_file: Some("/etc/thepost/siem-client.pem".into()),
                    ..InternalTls::default()
                },
                ..ok.clone()
            },
            SyslogConfig {
                tls: InternalTls {
                    key: Some(key_ref()),
                    ..InternalTls::default()
                },
                ..ok.clone()
            },
        ] {
            assert!(matches!(bad.check(), Err(Error::Invalid(_))), "{bad:?}");
        }
        let cfg: AuditExportConfig = serde_json::from_value(serde_json::json!({
            "retry": "1s",
            "syslog": {"address": "siem:6514", "tls": {"server_name": "siem.example.org"}},
            "otlp": {}
        }))
        .expect("parse");
        assert_eq!(cfg.retry, Duration::from_secs(1));
        assert_eq!(cfg.batch, 256);
        assert_eq!(
            cfg.otlp.as_ref().map(|o| o.endpoint.as_str()),
            Some("http://localhost:4318/v1/logs")
        );
        cfg.check().expect("check");
        assert!(cfg.enabled());
        assert!(!AuditExportConfig::default().enabled());
        for bad in [
            serde_json::json!({"kafka": {}}),
            // Очереди отправки нет: записи ждут в журнале (решение владельца 12.11).
            serde_json::json!({"syslog": {"address": "siem:6514", "queue": 10}}),
        ] {
            assert!(serde_json::from_value::<AuditExportConfig>(bad).is_err());
        }
        for bad in [
            AuditExportConfig {
                batch: 0,
                ..cfg.clone()
            },
            AuditExportConfig {
                retry: Duration::ZERO,
                ..cfg.clone()
            },
        ] {
            assert!(matches!(bad.check(), Err(Error::Invalid(_))));
        }
        let cfg: AuditExportConfig = serde_json::from_value(serde_json::json!({
            "syslog": {"address": "siem:6514", "tls": {
                "cert_file": "/etc/thepost/siem-client.pem",
                "key": {"secret": "thepost/tls/siem#key"}
            }}
        }))
        .expect("parse key");
        cfg.check().expect("check key");
    }

    fn key_ref() -> SecretRef {
        SecretRef::new("thepost/tls/siem", "key").expect("ref")
    }

    fn secrets() -> MemSecretStore {
        MemSecretStore::new(Arc::new(tp_testkit::SystemClock))
    }

    fn testdata(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/testdata")
            .join(name)
    }

    /// Номера и маршруты из кадров RFC 5425, пока не придёт `n` кадров.
    async fn read_frames(
        sock: &mut (impl tokio::io::AsyncRead + Unpin),
        n: usize,
    ) -> Vec<(u64, String)> {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let len = tokio::time::timeout_at(deadline, sock.read(&mut buf))
                .await
                .expect("timely")
                .expect("read");
            assert!(len > 0, "connection closed");
            got.extend_from_slice(&buf[..len]);
            let mut rest = got.as_slice();
            let mut frames = Vec::new();
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
                frames.push((
                    v["seq"].as_u64().expect("seq"),
                    v["route"].as_str().expect("route").to_owned(),
                ));
                rest = &rest[sp + 1 + len..];
            }
            if frames.len() == n {
                return frames;
            }
        }
    }

    fn plaintext(address: String) -> SyslogConfig {
        SyslogConfig {
            address,
            tls: InternalTls {
                allow_plaintext: true,
                ..InternalTls::default()
            },
            connect_timeout: Duration::from_secs(1),
            ..SyslogConfig::default()
        }
    }

    /// Открытый канал — только dev; кадры RFC 5425 приходят приёмнику по порядку.
    #[tokio::test]
    async fn syslog_delivers_frames() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let cfg = plaintext(listener.local_addr().expect("addr").to_string());
        let store = secrets();
        assert!(matches!(
            SyslogAudit::start(cfg.clone(), Profile::Prod, &store).await,
            Err(Error::Invalid(_))
        ));
        let sink = SyslogAudit::start(cfg, Profile::Dev, &store)
            .await
            .expect("start");
        let accept = tokio::spawn(async move { listener.accept().await.expect("accept").0 });
        sink.deliver(&[entry(1, "/one"), entry(2, "/два")])
            .await
            .expect("deliver");
        let mut sock = accept.await.expect("join");
        assert_eq!(
            read_frames(&mut sock, 2).await,
            [(1, "/one".to_owned()), (2, "/два".to_owned())]
        );
    }

    /// Приёмник недоступен — пачка не доставлена, отметка на месте; после возврата приёмника все записи доставлены
    /// по порядку, без повторов.
    #[tokio::test]
    async fn export_catches_up_after_outage() {
        let address = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            l.local_addr().expect("addr")
        };
        let sink = SyslogAudit::start(plaintext(address.to_string()), Profile::Dev, &secrets())
            .await
            .expect("start");
        let log = MemAuditLog::new();
        for i in 0..10 {
            log.append(&record(&format!("/r{i}")))
                .await
                .expect("append");
        }
        assert_eq!(export_pass(&log, &sink, 4).await, Err(Error::UNAVAILABLE));
        assert_eq!(log.export_mark("syslog").await, Ok(0));

        let listener = tokio::net::TcpListener::bind(address)
            .await
            .expect("rebind");
        let accept = tokio::spawn(async move { listener.accept().await.expect("accept").0 });
        let mut delivered = 0;
        loop {
            match export_pass(&log, &sink, 4).await.expect("pass") {
                0 => break,
                n => delivered += n,
            }
        }
        assert_eq!(delivered, 10);
        assert_eq!(log.export_mark("syslog").await, Ok(10));
        let mut sock = accept.await.expect("join");
        let frames = read_frames(&mut sock, 10).await;
        let expected: Vec<(u64, String)> = (0..10).map(|i| (i + 1, format!("/r{i}"))).collect();
        assert_eq!(frames, expected);
        assert_eq!(export_pass(&log, &sink, 4).await, Ok(0));
    }

    /// mTLS: ключ клиента — из хранилища секретов (`tls.key`); приёмник проверяет сертификат клиента.
    #[tokio::test]
    async fn syslog_tls_key_from_secrets() {
        use rustls::server::WebPkiClientVerifier;

        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(read_certs(&testdata("ca.pem")).expect("ca"));
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                .build()
                .expect("verifier");
        let server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                read_certs(&testdata("server.pem")).expect("server cert"),
                PrivateKeyDer::from_pem_file(testdata("server-key.pem")).expect("server key"),
            )
            .expect("server config");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let cfg = SyslogConfig {
            address: listener.local_addr().expect("addr").to_string(),
            tls: InternalTls {
                ca_file: Some(testdata("ca.pem")),
                cert_file: Some(testdata("client.pem")),
                key: Some(key_ref()),
                server_name: Some("siem.test".into()),
                ..InternalTls::default()
            },
            ..SyslogConfig::default()
        };
        let store = secrets();
        // Нет секрета — ошибка хранилища; не PEM — ошибка без значения в тексте.
        assert_eq!(
            SyslogAudit::start(cfg.clone(), Profile::Prod, &store)
                .await
                .err(),
            Some(Error::NotFound)
        );
        store.set(&key_ref(), "not-a-key-s3cr3t");
        match SyslogAudit::start(cfg.clone(), Profile::Prod, &store).await {
            Err(Error::Invalid(m)) => assert!(!m.contains("s3cr3t"), "{m}"),
            other => panic!("unexpected: {other:?}"),
        }
        store.set(&key_ref(), include_str!("testdata/client-key.pem"));
        let sink = SyslogAudit::start(cfg, Profile::Prod, &store)
            .await
            .expect("start");
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let mut sock = acceptor.accept(tcp).await.expect("handshake");
            assert!(
                sock.get_ref()
                    .1
                    .peer_certificates()
                    .is_some_and(|c| !c.is_empty()),
                "client certificate"
            );
            read_frames(&mut sock, 1).await
        });
        sink.deliver(&[entry(3, "/tls")]).await.expect("deliver");
        assert_eq!(server.await.expect("join"), [(3, "/tls".to_owned())]);
    }

    /// Экспорт в память: записи вызовов; `fail` — отказ экспорта.
    #[derive(Debug, Clone, Default)]
    struct Collect(Arc<Mutex<Vec<Exported>>>, Arc<Mutex<bool>>);

    /// Событие, тело, атрибуты.
    type Exported = (Option<&'static str>, String, Vec<(String, String)>);

    impl LogExporter for Collect {
        async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
            if *self.1.lock().expect("lock") {
                return Err(OTelSdkError::InternalFailure("down".into()));
            }
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
        let sink = OtlpAudit::with_exporter(collect.clone(), "thepost");
        *collect.1.lock().expect("lock") = true;
        assert_eq!(
            sink.deliver(&[entry(5, "/x")]).await,
            Err(Error::UNAVAILABLE)
        );
        *collect.1.lock().expect("lock") = false;
        sink.deliver(&[entry(5, "/api/v1/orgs/{org}/transport/domains")])
            .await
            .expect("deliver");
        let got = collect.0.lock().expect("lock").clone();
        assert_eq!(got.len(), 1);
        let (event, body, attrs) = &got[0];
        assert_eq!(*event, Some(OTLP_EVENT));
        let v: serde_json::Value = serde_json::from_str(body).expect("json");
        assert_eq!(v["route"], "/api/v1/orgs/{org}/transport/domains");
        assert_eq!(v["seq"], 5);
        for (key, value) in [
            ("actor_login", "admin@example.org"),
            ("status", "201"),
            ("seq", "5"),
        ] {
            assert!(
                attrs.iter().any(|(k, v)| k == key && v.contains(value)),
                "{key}: {attrs:?}"
            );
        }
    }

    #[test]
    fn otlp_http_builds_outside_runtime() {
        let sink = OtlpAudit::http(&OtlpAuditConfig::default()).expect("build");
        assert_eq!(sink.name(), "otlp");
    }
}
