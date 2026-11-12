//! Секции config.yaml приложения и реестр секций всех крейтов (SRS 5.5; APP-01, PLT-09).
//!
//! Приложение регистрирует секции полос (`database`, `meta`) и свои: `blob` — хранилище BLOB (локальный каталог
//! или S3), `bus` — шина уведомлений (в процессе), `cluster` — имя узла и опросы, `doctor` — пороги
//! проверок окружения, `startup` — ожидание хранилища секретов при запуске. Проверки открытых каналов подключаются через [`SectionRegistry::add_check`]: `thepost config
//! check` (строгий режим) и запуск (мягкий режим) отвергают адрес без TLS вне профиля dev.
//!
//! Подсекции `blob.local` и `blob.s3` — секции крейта `tp-blob` как есть (BLOB-11): приложение подставляет только каталог, бакет и префикс из `blob.url`.
//!
//! Секции ролей (APP-04) — секции крейтов как есть: `smtp`, `sieve`, `imap`, `jmap`, `dav`, `search`, `mailstore`; `admin` — секция
//! `tp-admin` без `listen` (адрес Admin API — один ключ `http.admin.listen`, APP-16); приложение добавляет `auth`
//! (каталог, OIDC, пароли приложений, Kerberos) и `http` (слушатели jmap, dav и admin).

use std::borrow::Cow;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer};
use tp_blob::{GcConfig, LocalConfig, OrphanConfig, S3Config};
use tp_cluster::{LocalBusConfig, RpcConfig};
use tp_contracts::config::duration;
use tp_contracts::role::{Role, RoleSet};
use tp_contracts::tls::{InternalChannel, Profile};
use tp_contracts::{Error, Result};
use tp_meta::MetaConfig;
use tp_meta::pool::PoolConfig;
use tp_platform::{SectionRegistry, ServiceHttpConfig};

use crate::modules;

/// Имя секции хранилища BLOB.
pub const BLOB: &str = "blob";
/// Имя секции шины уведомлений.
pub const BUS: &str = "bus";
/// Имя секции кластера.
pub const CLUSTER: &str = "cluster";
/// Имя секции порогов `thepost doctor`.
pub const DOCTOR: &str = "doctor";
/// Имя секции запуска узла.
pub const STARTUP: &str = "startup";
/// Имя секции `thepost verify`.
pub const VERIFY: &str = "verify";
/// Имя секции SMTP (`tp-smtp`).
pub const SMTP: &str = "smtp";
/// Имя секции IMAP (`tp-imap`).
pub const IMAP: &str = "imap";
/// Имя секции JMAP (`tp-jmap`).
pub const JMAP: &str = "jmap";
/// Имя секции календарей и контактов CalDAV/CardDAV (`tp-dav`).
pub const DAV: &str = "dav";
/// Имя секции правил Sieve и автоответа (`tp-sieve`).
pub const SIEVE: &str = "sieve";
/// Имя секции поиска (`tp-search`).
pub const SEARCH: &str = "search";
/// Имя секции Admin API (`tp-admin`).
pub const ADMIN: &str = "admin";
/// Имя секции почтового хранилища (`tp-mailstore`).
pub const MAILSTORE: &str = "mailstore";
/// Имя секции роли maintenance.
pub const MAINTENANCE: &str = "maintenance";
/// Имя секции входа: каталог, OIDC, пароли приложений, Kerberos.
pub const AUTH: &str = "auth";
/// Имя секции слушателей HTTP ролей jmap, dav и admin.
pub const HTTP: &str = "http";

/// Реестр секций: платформа, `tp-meta`, приложение и встроенные модули; проверки каналов.
pub fn registry() -> Result<SectionRegistry> {
    registry_of(modules::BUILTIN)
}

/// Как [`registry`] с секциями модулей `builtin` (тесты — свои модули).
pub fn registry_of(builtin: &[modules::Builtin]) -> Result<SectionRegistry> {
    let mut r = SectionRegistry::new();
    r.add_section::<PoolConfig>(PoolConfig::SECTION)?;
    r.add_section::<MetaConfig>(MetaConfig::SECTION)?;
    r.add_section::<BlobSection>(BLOB)?;
    r.add_section::<BusSection>(BUS)?;
    r.add_section::<ClusterSection>(CLUSTER)?;
    r.add_section::<DoctorSection>(DOCTOR)?;
    r.add_section::<VerifySection>(VERIFY)?;
    r.add_section::<StartupSection>(STARTUP)?;
    r.add_section::<MaintenanceSection>(MAINTENANCE)?;
    r.add_section::<tp_smtp::SmtpConfig>(SMTP)?;
    r.add_section::<tp_sieve::SieveConfig>(SIEVE)?;
    r.add_section::<tp_imap::ImapConfig>(IMAP)?;
    r.add_section::<tp_jmap::JmapConfig>(JMAP)?;
    r.add_section::<tp_dav::DavConfig>(DAV)?;
    r.add_section::<tp_search::SearchConfig>(SEARCH)?;
    r.add_section::<AdminSection>(ADMIN)?;
    r.add_section::<tp_mailstore::Config>(MAILSTORE)?;
    r.add_section::<AuthSection>(AUTH)?;
    r.add_section::<HttpSection>(HTTP)?;
    r.add_check::<PoolConfig>(PoolConfig::SECTION, |c, profile| {
        pool_config(c, profile).check().map(drop)
    })?;
    r.add_check::<BlobSection>(BLOB, BlobSection::check)?;
    r.add_check::<StartupSection>(STARTUP, |c, _| c.check())?;
    r.add_check::<MaintenanceSection>(MAINTENANCE, |c, _| c.check())?;
    r.add_check::<ClusterSection>(CLUSTER, |c, _| c.check())?;
    r.add_check::<AdminSection>(ADMIN, |c, _| c.0.check())?;
    r.add_check::<AuthSection>(AUTH, AuthSection::check)?;
    r.add_check::<HttpSection>(HTTP, HttpSection::check)?;
    modules::register_sections_of(&mut r, builtin)?;
    Ok(r)
}

/// Секция `database` с профилем узла (поле профиля задаёт приложение, в config.yaml его нет).
pub fn pool_config(c: &PoolConfig, profile: Profile) -> PoolConfig {
    PoolConfig {
        profile,
        ..c.clone()
    }
}

// ---------- blob ----------

/// Секция `blob`: хранилище объектов писем и индексов.
///
/// ```yaml
/// blob:
///   url: file:///var/lib/thepost/blob      # или s3://thepost-blob/prefix
///   s3:
///     endpoint: https://seaweedfs:8333
///     credentials:
///       access_key_id: { secret: "thepost/s3#access_key_id" }
///       secret_access_key: { secret: "thepost/s3#secret_access_key" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BlobSection {
    /// Адрес хранилища: `file:///<каталог>` — локальный каталог (в том числе общий для узлов),
    /// `s3://<бакет>[/<префикс>]` — S3 (адрес API — `s3.endpoint`). Требует перезапуска.
    pub url: String,
    /// Параметры локального каталога — секция `tp-blob`; каталог (`root`) задаётся в `url`.
    pub local: LocalConfig,
    /// Параметры S3 — секция `tp-blob`; бакет и префикс (`bucket`, `prefix`) задаются в `url`.
    pub s3: S3Config,
    /// Сборка мусора объектов (роль maintenance) — секция `tp-blob`.
    pub gc: GcConfig,
    /// Поиск «сирот» (роль maintenance) — секция `tp-blob`.
    pub orphans: OrphanConfig,
    /// Сжатие объектов писем zstd при записи (BLOB-13, MS-15) — секция `tp-blob`.
    pub compression: tp_blob::CompressionConfig,
}

/// Где лежат объекты: разобранный `blob.url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobLocation {
    /// Локальный каталог.
    Local(PathBuf),
    /// Бакет S3 и префикс ключей.
    S3 {
        /// Бакет.
        bucket: String,
        /// Префикс ключей; пустой — без префикса.
        prefix: String,
    },
}

impl BlobSection {
    /// Разбор `url`.
    pub fn location(&self) -> Result<BlobLocation> {
        let url = self.url.trim();
        if let Some(path) = url.strip_prefix("file://") {
            if !path.starts_with('/') {
                return Err(Error::invalid(
                    "blob.url: путь file:// должен быть абсолютным (file:///var/lib/thepost/blob)",
                ));
            }
            return Ok(BlobLocation::Local(PathBuf::from(path)));
        }
        if let Some(rest) = url.strip_prefix("s3://") {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            if bucket.is_empty() {
                return Err(Error::invalid("blob.url: не задан бакет s3://<бакет>"));
            }
            return Ok(BlobLocation::S3 {
                bucket: bucket.to_owned(),
                prefix: prefix.trim_matches('/').to_owned(),
            });
        }
        Err(Error::invalid(
            "blob.url: ожидается file:///<каталог> или s3://<бакет>[/<префикс>]",
        ))
    }

    /// Конфигурация локального хранилища для каталога `root` из `url`.
    pub fn local_config(&self, root: PathBuf) -> LocalConfig {
        LocalConfig {
            root,
            ..self.local.clone()
        }
    }

    /// Конфигурация S3 для бакета и префикса из `url`.
    pub fn s3_config(&self, bucket: String, prefix: String) -> S3Config {
        S3Config {
            bucket,
            prefix,
            ..self.s3.clone()
        }
    }

    /// Проверка секции: адрес, TLS канала S3 (`https://`; `http://` — только dev с `s3.tls.allow_plaintext`);
    /// каталог, бакет и префикс — только в `url`.
    pub fn check(&self, profile: Profile) -> Result<()> {
        self.s3.tls.validate(InternalChannel::S3, profile)?;
        if self.local.root != LocalConfig::default().root {
            return Err(Error::invalid(
                "local.root: каталог задаётся в blob.url (file:///<каталог>)",
            ));
        }
        if self.s3.bucket != S3Config::default().bucket || !self.s3.prefix.is_empty() {
            return Err(Error::invalid(
                "s3.bucket, s3.prefix: бакет и префикс задаются в blob.url (s3://<бакет>/<префикс>)",
            ));
        }
        match self.location()? {
            BlobLocation::Local(_) => Ok(()),
            BlobLocation::S3 { bucket, prefix } => {
                self.s3_config(bucket, prefix).check(profile).map(drop)
            }
        }
    }
}

// ---------- bus ----------

/// Реализация шины уведомлений.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BusKind {
    /// В процессе: один узел (SMB).
    #[default]
    Local,
    /// PostgreSQL LISTEN/NOTIFY (`tp_meta::signals::PgSignalBus`): кластер из нескольких узлов. Канал, буферы и
    /// очередь публикатора — секция `meta.signals`; слушатель — прямое соединение с primary (с PgBouncer —
    /// `database.direct_url`).
    Postgres,
}

/// Секция `bus`: шина сигналов (SRS 2.8). Требует перезапуска. Шина Valkey удалена (CLU-09).
///
/// ```yaml
/// bus:
///   kind: local      # postgres — кластер (LISTEN/NOTIFY, параметры — meta.signals)
///   capacity: 256
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BusSection {
    /// Реализация: `local` — в процессе, `postgres` — LISTEN/NOTIFY.
    pub kind: BusKind,
    /// Буфер подписчика шины в процессе: столько непрочитанных уведомлений до отключения отставшего (`local`).
    pub capacity: usize,
}

impl Default for BusSection {
    fn default() -> Self {
        Self {
            kind: BusKind::Local,
            capacity: LocalBusConfig::default().capacity,
        }
    }
}

impl BusSection {
    /// Конфигурация шины в процессе.
    pub fn local(&self) -> LocalBusConfig {
        LocalBusConfig {
            capacity: self.capacity,
        }
    }
}

// ---------- cluster ----------

/// Секция `cluster`: узел в составе кластера (членство, разделы журнала событий, запросы узел → узел).
///
/// ```yaml
/// cluster:
///   node_id: mail-1
///   site: dc-1
///   addr: https://mail-1.corp.example:7443   # адрес узла для других узлов; пусто — запросов не принимает
///   listen: 0.0.0.0:7443                     # где узел слушает; не задан — как в addr
///   rpc:                                     # секция tp-cluster (CLU-12): mTLS внутреннего канала
///     tls:
///       ca_file: /etc/thepost/cluster-ca.pem
///       cert_file: /etc/thepost/node.pem     # CN — node_id
///       key_file: /etc/thepost/node-key.pem  # либо key: { secret: "thepost/node#key" } — ключ из хранилища секретов
///     permissions:
///       jmap: [search/query]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ClusterSection {
    /// Имя узла в членстве кластера, постоянное для узла; не задано — имя хоста. Требует перезапуска.
    pub node_id: Option<String>,
    /// Площадка (ЦОД) узла в членстве кластера; по умолчанию `default` (одна площадка).
    pub site: String,
    /// Внутренний адрес служебного канала узла для запросов узел → узел (`https://host:port`): объявляется в
    /// записи узла, по нему другие узлы вызывают этот; пусто — узел запросов не принимает. Требует `rpc.tls` и
    /// перезапуска.
    pub addr: String,
    /// Адрес приёма запросов узел → узел (`host:port`, `0.0.0.0:port`, `[::]:port`): на нём узел слушает
    /// (`NodeRpc::listen`), другим узлам объявляется `addr` — за NAT, пробросом порта или балансировщиком они
    /// различаются. Не задан — как в `addr` (решение 36). Требует `addr` и перезапуска.
    pub listen: String,
    /// Запросы узел → узел (секция `tp-cluster`, CLU-12): пределы, сроки, матрица разрешений и mTLS внутреннего
    /// канала (`rpc.tls`: УЦ узлов, сертификат с CN — имя узла, ключ — файл `key_file` либо ссылка на секрет
    /// `key`). Транспорт включается адресом `addr` или сертификатом узла `rpc.tls.cert_file` (только исходящие
    /// запросы).
    pub rpc: RpcConfig,
    /// Период проверки сертификатов внутреннего канала `rpc.tls` (файлы УЦ, сертификата и ключа, секрет `key`):
    /// изменение применяется без перезапуска, ошибка новой пары оставляет прежнюю. `0s` — не проверять (замена
    /// сертификата — перезапуском узла).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub rpc_tls_reload_interval: Duration,
    /// Запросов перестройки индексов, просматриваемых ролью indexer за проход (перестраиваются ящики своих
    /// разделов журнала).
    pub rebuild_scan: usize,
}

impl Default for ClusterSection {
    fn default() -> Self {
        Self {
            node_id: None,
            site: tp_contracts::membership::SiteId::DEFAULT.to_owned(),
            addr: String::new(),
            listen: String::new(),
            rpc: RpcConfig::default(),
            rpc_tls_reload_interval: Duration::from_secs(60),
            rebuild_scan: 64,
        }
    }
}

impl ClusterSection {
    /// Площадка узла.
    pub fn site_id(&self) -> Result<tp_contracts::membership::SiteId> {
        tp_contracts::membership::SiteId::new(self.site.clone())
            .map_err(|e| Error::invalid(format!("site: {e}")))
    }

    /// Проверка: имя узла (если задано), площадка, адрес (пусто или `https://host:port`), адрес приёма (пусто или
    /// `host:port` с портом 1–65535, только вместе с `addr`), секция `rpc`; с
    /// включённым транспортом ([`ClusterSection::rpc_enabled`]) — `rpc.tls`: файлы УЦ и сертификата, ключ — файл
    /// `key_file` либо ссылка на секрет `key` (вместе не задаются).
    pub fn check(&self) -> Result<()> {
        if let Some(node) = &self.node_id {
            tp_contracts::membership::NodeId::new(node.clone())?;
        }
        self.site_id()?;
        if !self.addr.is_empty()
            && self
                .addr
                .strip_prefix("https://")
                .and_then(host_port)
                .is_none()
        {
            return Err(Error::invalid(format!(
                "addr: ожидается https://host:port, получено {:?}",
                self.addr
            )));
        }
        if !self.listen.is_empty() {
            if host_port(&self.listen).is_none() {
                return Err(Error::invalid(format!(
                    "listen: ожидается host:port с портом 1–65535 (0.0.0.0:7443), получено {:?}",
                    self.listen
                )));
            }
            if self.addr.is_empty() {
                return Err(Error::invalid(
                    "listen: задан без addr — другим узлам адрес этого узла не объявлен",
                ));
            }
        }
        if self.rpc_enabled() {
            let tls = &self.rpc.tls;
            // Ключ — файл либо секрет (CTR-52): источник определяет контракт, оба сразу — ошибка.
            let key = tls.key_source().map_err(|e| match e {
                Error::Invalid(m) => Error::invalid(format!("rpc.{m}")),
                other => other,
            })?;
            for (field, set) in [
                ("ca_file", tls.ca_file.is_some()),
                ("cert_file", tls.cert_file.is_some()),
                ("key_file либо rpc.tls.key", key.is_some()),
            ] {
                if !set {
                    return Err(Error::invalid(format!(
                        "rpc.tls.{field}: обязателен для запросов узел → узел (mTLS; задан addr или rpc.tls.cert_file)"
                    )));
                }
            }
        }
        self.rpc.check()
    }

    /// Транспорт запросов узел → узел включён: задан адрес узла или его сертификат.
    pub fn rpc_enabled(&self) -> bool {
        !self.addr.is_empty() || self.rpc.tls.cert_file.is_some()
    }

    /// Адрес слушателя запросов узел → узел: `listen`, не задан — `host:port` из `addr`; `None` — адреса нет или он
    /// неверен.
    pub fn listen_addr(&self) -> Option<&str> {
        if self.listen.is_empty() {
            self.addr.strip_prefix("https://").and_then(host_port)
        } else {
            host_port(&self.listen)
        }
    }

    /// Ключ, из которого взят адрес слушателя (`cluster.listen` или `cluster.addr`), — для сообщений об ошибках.
    pub fn listen_key(&self) -> &'static str {
        if self.listen.is_empty() {
            "cluster.addr"
        } else {
            "cluster.listen"
        }
    }
}

/// `host:port` без схемы и пути, имя непустое, порт 1–65535; иначе `None`.
fn host_port(s: &str) -> Option<&str> {
    let (name, port) = s.rsplit_once(':')?;
    let valid = !name.is_empty()
        && !s.contains(['/', '?', '#'])
        && !s.chars().any(char::is_whitespace)
        && port.parse::<u16>().is_ok_and(|p| p != 0);
    valid.then_some(s)
}

// ---------- auth ----------

/// Секция `auth`: вход в протоколы (SRS 6.1) — каталог LDAP, OIDC (Bearer), пароли приложений, Kerberos, защита от
/// перебора.
///
/// ```yaml
/// auth:
///   ldap:
///     url: ldaps://dc.example.ru
///     bind_dn: cn=thepost,ou=services,dc=example,dc=ru
///     bind_password: { secret: "thepost/ldap#password" }
///     base_dn: dc=example,dc=ru
///   oidc:
///     issuer: https://sso.example.ru/realms/mail
///     audience: [thepost]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AuthSection {
    /// Каталог LDAP / Active Directory (`tp-auth`): адреса, группы, проверка пароля привязкой; не задан —
    /// учётные записи только из PostgreSQL, вход — паролями приложений. Требует перезапуска.
    pub ldap: Option<tp_auth::LdapConfig>,
    /// Поставщик OpenID Connect: токены Bearer (JMAP, Admin API, SASL OAUTHBEARER). Требует перезапуска.
    pub oidc: Option<tp_auth::OidcConfig>,
    /// Пароли приложений (argon2id).
    pub passwords: tp_auth::PasswordConfig,
    /// Kerberos (SASL GSSAPI). Keytab задаёт переменная `KRB5_KTNAME` (systemd `Environment=`): поле `keytab`
    /// только сверяется с ней. Требует перезапуска.
    pub gssapi: Option<tp_auth::GssapiConfig>,
    /// Защита от перебора (`tp-auth`, AUTH-08): пороги неудачных входов, срок блокировки. Требует перезапуска.
    pub abuse: tp_auth::AbuseConfig,
}

impl AuthSection {
    /// Проверка: каналы LDAP и OIDC (TLS вне dev), keytab Kerberos.
    pub fn check(&self, profile: Profile) -> Result<()> {
        if let Some(ldap) = &self.ldap {
            ldap.check(profile).map_err(|e| sub("ldap", e))?;
        }
        if let Some(oidc) = &self.oidc {
            oidc.check(profile).map_err(|e| sub("oidc", e))?;
        }
        if let Some(gssapi) = &self.gssapi {
            gssapi.check().map_err(|e| sub("gssapi", e))?;
        }
        self.abuse.check()?;
        Ok(())
    }
}

// ---------- http ----------

/// Секция `http`: слушатели HTTP ролей jmap, dav и admin (TLS — секция `tls`; открытый HTTP — только `profile: dev`
/// с `allow_plaintext`). Слушатель не задан — роль готова, но соединений не принимает. Требует перезапуска.
/// Календари и контакты (роль dav, `/dav/*`) без `dav` отдаёт слушатель `jmap` — один порт 443 с JMAP.
/// `acme` — открытый HTTP для проверок ACME HTTP-01 (только при `acme.enabled`, APP-14).
///
/// ```yaml
/// http:
///   jmap: { listen: 0.0.0.0:443 }
///   admin: { listen: 127.0.0.1:8443 }
///   acme: { listen: 0.0.0.0:80 }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HttpSection {
    /// JMAP (`/.well-known/jmap`, `/jmap/*`).
    pub jmap: Option<HttpListener>,
    /// CalDAV и CardDAV (`/dav/*`, `/.well-known/caldav`, `/.well-known/carddav`) на отдельном адресе; не задан —
    /// на слушателе `jmap`.
    pub dav: Option<HttpListener>,
    /// Admin API (`/api/*`); адрес — только внутренний (loopback, частные сети).
    pub admin: Option<HttpListener>,
    /// Проверки ACME HTTP-01 (`/.well-known/acme-challenge/*`, остальное — 404) при `acme.enabled`.
    pub acme: AcmeListener,
}

/// Слушатель проверок ACME HTTP-01: открытый HTTP (сервер ACME обращается на порт 80, RFC 8555 §8.3).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AcmeListener {
    /// Адрес и порт.
    pub listen: SocketAddr,
}

impl Default for AcmeListener {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 80)),
        }
    }
}

/// Слушатель HTTP роли.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HttpListener {
    /// Адрес и порт.
    pub listen: SocketAddr,
    /// Открытый HTTP без TLS: только `profile: dev`.
    pub allow_plaintext: bool,
    /// Время на TLS-рукопожатие.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub handshake_timeout: Duration,
    /// Одновременных незавершённых TLS-рукопожатий.
    pub max_handshakes: usize,
}

impl Default for HttpListener {
    fn default() -> Self {
        let base = ServiceHttpConfig::default();
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 443)),
            allow_plaintext: false,
            handshake_timeout: base.handshake_timeout,
            max_handshakes: base.max_handshakes,
        }
    }
}

impl HttpListener {
    /// Параметры сервера HTTP платформы (`tp_platform::service_http::serve`). `public` — роль для клиентов
    /// (jmap): адрес слушателя разрешён явно; иначе (admin) — только внутренние адреса, как у служебного HTTP.
    pub fn server(&self, public: bool) -> ServiceHttpConfig {
        let ip = self.listen.ip();
        let prefix = if ip.is_ipv4() { 32 } else { 128 };
        ServiceHttpConfig {
            listen: self.listen,
            internal_networks: if public {
                vec![tp_platform::Cidr::new(ip, prefix)]
            } else {
                Vec::new()
            },
            allow_plaintext: self.allow_plaintext,
            handshake_timeout: self.handshake_timeout,
            max_handshakes: self.max_handshakes,
        }
    }
}

impl HttpSection {
    /// Проверка: открытый HTTP — только в dev, ненулевые таймауты, Admin API — только на внутреннем адресе,
    /// `dav` — не тот же адрес, что `jmap` (общий порт — без `http.dav`).
    pub fn check(&self, profile: Profile) -> Result<()> {
        for (name, l, public) in [
            ("jmap", &self.jmap, true),
            ("dav", &self.dav, true),
            ("admin", &self.admin, false),
        ] {
            if let Some(l) = l {
                l.server(public).check(profile).map_err(|e| sub(name, e))?;
            }
        }
        if let (Some(jmap), Some(dav)) = (&self.jmap, &self.dav)
            && jmap.listen == dav.listen
            && jmap.listen.port() != 0
        {
            return Err(Error::Invalid(format!(
                "dav: listen {} совпадает с http.jmap.listen: для общего порта уберите http.dav — календари и \
                 контакты отдаёт слушатель jmap",
                dav.listen
            )));
        }
        Ok(())
    }

    /// Слушатель календарей и контактов узла с ролями `roles`: `http.dav`; не задан — `http.jmap`. Второе
    /// значение — `true`, если это слушатель роли jmap того же узла (маршруты DAV добавляются к JMAP, своего
    /// сокета у роли dav нет).
    pub fn dav_listener(&self, roles: &RoleSet) -> (Option<&HttpListener>, bool) {
        match &self.dav {
            Some(dav) => (Some(dav), false),
            None => (self.jmap.as_ref(), roles.contains(Role::Jmap)),
        }
    }
}

/// Ошибка подсекции с префиксом её имени.
fn sub(name: &str, e: Error) -> Error {
    match e {
        Error::Invalid(m) => Error::Invalid(format!("{name}: {m}")),
        other => other,
    }
}

// ---------- admin ----------

/// Подсказка для `admin.listen`: адрес Admin API задаёт только `http.admin.listen` (SRS, APP-16).
const ADMIN_LISTEN_HINT: &str = "admin.listen не используется: адрес Admin API задаётся в http.admin.listen \
     (например, http: { admin: { listen: 127.0.0.1:8443 } })";

/// Подсказка для `admin.allow_external`: ключ без действия — Admin API слушает только внутренний адрес
/// `http.admin.listen` (SRS 6.8), внешний адрес не разрешается ничем.
const ADMIN_ALLOW_EXTERNAL_HINT: &str = "admin.allow_external не используется: Admin API принимает соединения \
     только на внутреннем адресе http.admin.listen (loopback или частная сеть, SRS 6.8); внешний доступ — через \
     обратный прокси или VPN";

/// Ключи `tp-admin`, которые приложение не применяет, и подсказки к ним.
const ADMIN_REJECTED: [(&str, &str); 2] = [
    ("listen", ADMIN_LISTEN_HINT),
    ("allow_external", ADMIN_ALLOW_EXTERNAL_HINT),
];

/// Секция `admin`: секция `tp-admin` как есть, кроме `listen` — адрес слушателя задаёт `http.admin.listen`, ключ
/// `admin.listen` — ошибка разбора с подсказкой (два ключа одного адреса расходились: проверялся один, слушал другой);
/// `admin.allow_external` — тоже ошибка с подсказкой: ключ без действия (адрес проверяет `http.admin`, только внутренний).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdminSection(pub tp_admin::AdminConfig);

impl<'de> Deserialize<'de> for AdminSection {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let value = serde_json::Value::deserialize(d)?;
        if value.is_null() {
            return Ok(Self::default());
        }
        if let Some((_, hint)) = ADMIN_REJECTED
            .iter()
            .find(|(key, _)| value.get(key).is_some())
        {
            return Err(D::Error::custom(hint));
        }
        tp_admin::AdminConfig::deserialize(value)
            .map(Self)
            .map_err(D::Error::custom)
    }
}

impl JsonSchema for AdminSection {
    fn schema_name() -> Cow<'static, str> {
        "AdminSection".into()
    }

    /// Схема `tp-admin`; `listen` и `allow_external` остаются известными ключами (иначе мягкий режим отбросил бы
    /// их с предупреждением), но не допускают значений.
    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut schema = tp_admin::AdminConfig::json_schema(generator);
        for (key, hint) in ADMIN_REJECTED {
            if let Some(prop) = schema.get_mut("properties").and_then(|p| p.get_mut(key)) {
                *prop = serde_json::json!({ "description": hint, "not": {} });
            }
        }
        schema
    }
}

// ---------- verify ----------

/// Секция `verify`: сверка хранилища BLOB и учёта объектов (`thepost verify`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct VerifySection {
    /// Записей учёта на страницу перечисления (каждая страница — своя короткая транзакция).
    pub page_size: usize,
}

impl Default for VerifySection {
    fn default() -> Self {
        Self { page_size: 1000 }
    }
}

// ---------- startup ----------

/// Секция `startup`: ожидание внешних служб при запуске узла (APP-31, решение 2026-11-11 п. 58).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StartupSection {
    /// Сколько узел ждёт хранилище секретов (OpenBao) при запуске, повторяя вход и чтение секретов (`120s`);
    /// `0s` — без повторов: первая неудача завершает запуск.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub secrets_timeout: Duration,
    /// Пауза перед первым повтором (`1s`); дальше удваивается.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub secrets_retry_initial: Duration,
    /// Наибольшая пауза между повторами (`15s`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub secrets_retry_max: Duration,
}

impl Default for StartupSection {
    fn default() -> Self {
        Self {
            secrets_timeout: Duration::from_secs(120),
            secrets_retry_initial: Duration::from_secs(1),
            secrets_retry_max: Duration::from_secs(15),
        }
    }
}

impl StartupSection {
    /// Паузы положительны, наибольшая — не меньше первой.
    pub fn check(&self) -> Result<()> {
        if self.secrets_retry_initial.is_zero()
            || self.secrets_retry_max < self.secrets_retry_initial
        {
            return Err(Error::invalid(
                "startup: secrets_retry_initial must be positive and not exceed secrets_retry_max",
            ));
        }
        Ok(())
    }
}

// ---------- maintenance ----------

/// Секция `maintenance`: задания роли maintenance, у которых нет своей секции крейта (APP-32, решение 2026-11-11
/// п. 65). Сроки хранения — в `meta` (`meta.dav.tombstone_ttl`, `meta.sieve.vacation_log_ttl`).
///
/// ```yaml
/// maintenance:
///   purge_interval: 1h
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenanceSection {
    /// Период очистки по сроку надгробий DAV и журнала автоответов (`1h`); первый проход — при запуске роли.
    /// Требует перезапуска.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub purge_interval: Duration,
}

impl Default for MaintenanceSection {
    fn default() -> Self {
        Self {
            purge_interval: Duration::from_secs(3600),
        }
    }
}

impl MaintenanceSection {
    /// Период положителен.
    pub fn check(&self) -> Result<()> {
        if self.purge_interval.is_zero() {
            return Err(Error::invalid(
                "maintenance: purge_interval must be positive",
            ));
        }
        Ok(())
    }
}

// ---------- doctor ----------

/// Секция `doctor`: пороги проверок окружения `thepost doctor` (проверки СУБД — `meta.doctor`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DoctorSection {
    /// Расхождение часов узла и СУБД, с которого выдаётся предупреждение.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub warn_clock_skew: Duration,
    /// Расхождение часов узла и СУБД, с которого проверка не пройдена (лизы, сроки записей, DKIM, TOTP).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub max_clock_skew: Duration,
    /// Предел ожидания каждой проверки хранилища BLOB.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub blob_timeout: Duration,
    /// Предел ожидания каждого запроса DNS (MX, SPF, DKIM, DMARC, MTA-STS, PTR).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub dns_timeout: Duration,
    /// Сертификату `tls` осталось действовать меньше — предупреждение.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub cert_warn_before: Duration,
}

impl Default for DoctorSection {
    fn default() -> Self {
        Self {
            warn_clock_skew: Duration::from_millis(500),
            max_clock_skew: Duration::from_secs(5),
            blob_timeout: Duration::from_secs(10),
            dns_timeout: Duration::from_secs(5),
            cert_warn_before: Duration::from_secs(21 * 86_400),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<tp_platform::Config> {
        registry().expect("registry").parse(yaml, Vec::new())
    }

    fn strict(yaml: &str) -> Result<tp_platform::Config> {
        registry().expect("registry").parse_strict(yaml, Vec::new())
    }

    fn err(r: Result<tp_platform::Config>) -> String {
        match r {
            Ok(c) => panic!("ожидалась ошибка: {c:?}"),
            Err(Error::Invalid(m)) => m,
            Err(e) => e.to_string(),
        }
    }

    /// Минимальная конфигурация dev; секция `blob` — последней (тесты дописывают её подсекции).
    const MINIMAL_DEV: &str = "
domain: example.org
profile: dev
service_http:
  allow_plaintext: true
database:
  url: postgres://thepost@localhost/thepost?sslmode=disable
  tls: { allow_plaintext: true }
blob:
  url: file:///var/lib/thepost/blob
";

    #[test]
    fn minimal_config_parses() {
        let c = parse(MINIMAL_DEV).expect("config");
        assert_eq!(c.domain(), "example.org");
        assert_eq!(c.profile(), Profile::Dev);
        let blob = c.section::<BlobSection>(BLOB).expect("blob");
        assert_eq!(
            blob.location().expect("location"),
            BlobLocation::Local(PathBuf::from("/var/lib/thepost/blob"))
        );
        assert_eq!(
            *c.section::<BusSection>(BUS).expect("bus"),
            BusSection::default()
        );
        assert!(c.roles().contains(tp_contracts::role::Role::Maintenance));
        strict(MINIMAL_DEV).expect("strict");
    }

    #[test]
    fn strict_rejects_unknown_fields_lenient_warns() {
        let yaml = format!("{MINIMAL_DEV}bus:\n  capasity: 3\nblob_typo: 1\n");
        let e = err(strict(&yaml));
        assert!(e.contains("bus.capasity"), "{e}");
        assert!(e.contains("blob_typo"), "{e}");
        let c = parse(&yaml).expect("lenient");
        assert_eq!(c.warnings().len(), 2, "{:?}", c.warnings());
    }

    #[test]
    fn plaintext_channels_only_in_dev() {
        let prod = |extra: &str| {
            format!(
                "domain: example.org\ndatabase:\n  url: postgres://thepost@db/thepost\n\
                 blob:\n  url: s3://bucket\n  s3:\n    endpoint: https://s3.internal\n{extra}"
            )
        };
        parse(&prod("")).expect("tls everywhere");
        // PostgreSQL без TLS.
        let e = err(parse(
            &prod("").replace("/thepost\n", "/thepost?sslmode=disable\n"),
        ));
        assert!(e.contains("database"), "{e}");
        // S3 по http.
        let e = err(parse(&prod("").replace("https://s3", "http://s3")));
        assert!(e.contains("blob"), "{e}");
        // Секции Valkey нет (CLU-09).
        let e = err(parse(&prod(
            "bus:\n  kind: valkey\n  valkey:\n    url: rediss://v:6379\n",
        )));
        assert!(e.contains("valkey"), "{e}");
    }

    #[test]
    fn secrets_only_by_reference() {
        let keys = |key: &str| {
            format!(
                "{MINIMAL_DEV}  s3:\n    credentials:\n      access_key_id: {{ secret: \"thepost/s3#id\" }}\n      secret_access_key: {key}\n"
            )
        };
        let e = err(parse(&keys("hunter2")));
        assert!(!e.contains("hunter2"), "{e}");
        assert!(e.contains("secret"), "{e}");
        let c = parse(&keys("{ secret: \"thepost/s3#key\" }")).expect("reference");
        let blob = c.section::<BlobSection>(BLOB).expect("blob");
        assert_eq!(
            blob.s3
                .credentials
                .as_ref()
                .map(|k| k.secret_access_key.to_string()),
            Some("thepost/s3#key".to_owned())
        );
        let refs: Vec<String> = c.secret_refs().map(ToString::to_string).collect();
        assert_eq!(refs.len(), 2, "{refs:?}");
    }

    #[test]
    fn blob_locations() {
        let loc = |url: &str| {
            BlobSection {
                url: url.to_owned(),
                ..BlobSection::default()
            }
            .location()
        };
        assert_eq!(
            loc("s3://bucket/a/b/").expect("s3"),
            BlobLocation::S3 {
                bucket: "bucket".into(),
                prefix: "a/b".into()
            }
        );
        assert_eq!(
            loc("s3://bucket").expect("s3"),
            BlobLocation::S3 {
                bucket: "bucket".into(),
                prefix: String::new()
            }
        );
        for bad in ["", "file://relative", "s3://", "/var/lib", "http://x"] {
            assert!(loc(bad).is_err(), "{bad}");
        }
    }

    /// mTLS внутреннего канала узла: файлы читаются при запуске транспорта, здесь — только наличие.
    const RPC_TLS: &str = "  rpc:\n    tls:\n      ca_file: /etc/thepost/ca.pem\n      \
                           cert_file: /etc/thepost/node.pem\n      key_file: /etc/thepost/node-key.pem\n";

    #[test]
    fn cluster_site_and_addr_are_checked() {
        let c = parse(&format!(
            "{MINIMAL_DEV}cluster:\n  site: dc-1\n  addr: https://10.0.0.1:7443\n{RPC_TLS}"
        ))
        .expect("config");
        let cluster = c.section::<ClusterSection>(CLUSTER).expect("cluster");
        assert_eq!(cluster.site_id().expect("site").as_str(), "dc-1");
        assert_eq!(cluster.addr, "https://10.0.0.1:7443");
        assert_eq!(cluster.listen_addr(), Some("10.0.0.1:7443"));
        assert_eq!(cluster.listen_key(), "cluster.addr");
        assert!(cluster.rpc_enabled());
        let d = ClusterSection::default();
        assert_eq!(d.site_id().expect("site").as_str(), "default");
        assert!(d.addr.is_empty());
        assert_eq!(d.listen_addr(), None);
        assert!(!d.rpc_enabled());
        for bad in [
            "  site: \"dc 1\"\n",
            "  site: \"\"\n",
            "  addr: http://x:1\n",
            "  addr: https://\n",
            "  addr: https://x/p\n",
            "  addr: https://x\n",
            "  addr: https://x:0\n",
            "  addr: https://:7443\n",
        ] {
            let e = err(parse(&format!("{MINIMAL_DEV}cluster:\n{bad}{RPC_TLS}")));
            assert!(e.contains("site") || e.contains("addr"), "{bad}: {e}");
        }
    }

    /// APP-30: `cluster.listen` — где узел слушает, `cluster.addr` — что объявляет; без `listen` слушает `addr`
    /// (решение 36); порт обязателен, `listen` без `addr` — ошибка.
    #[test]
    fn cluster_listen_is_separate_from_addr() {
        let addr = "  addr: https://mail-1.corp.example:7443\n";
        let c = parse(&format!(
            "{MINIMAL_DEV}cluster:\n{addr}  listen: 0.0.0.0:7443\n{RPC_TLS}"
        ))
        .expect("config");
        let cluster = c.section::<ClusterSection>(CLUSTER).expect("cluster");
        assert_eq!(cluster.addr, "https://mail-1.corp.example:7443");
        assert_eq!(cluster.listen_addr(), Some("0.0.0.0:7443"));
        assert_eq!(cluster.listen_key(), "cluster.listen");
        let c = parse(&format!(
            "{MINIMAL_DEV}cluster:\n{addr}  listen: \"[::]:17443\"\n{RPC_TLS}"
        ))
        .expect("config");
        let cluster = c.section::<ClusterSection>(CLUSTER).expect("cluster");
        assert_eq!(cluster.listen_addr(), Some("[::]:17443"));
        for bad in [
            "0.0.0.0",
            "0.0.0.0:0",
            "0.0.0.0:70000",
            ":7443",
            "https://0.0.0.0:7443",
            "0.0.0.0 :7443",
        ] {
            let e = err(parse(&format!(
                "{MINIMAL_DEV}cluster:\n{addr}  listen: \"{bad}\"\n{RPC_TLS}"
            )));
            assert!(
                e.contains("listen") && e.contains("host:port"),
                "{bad}: {e}"
            );
        }
        let e = err(parse(&format!(
            "{MINIMAL_DEV}cluster:\n  listen: 0.0.0.0:7443\n{RPC_TLS}"
        )));
        assert!(e.contains("listen") && e.contains("addr"), "{e}");
    }

    /// APP-18: адрес узла (или сертификат для исходящих запросов) требует mTLS внутреннего канала
    /// `cluster.rpc.tls`; открытого канала нет ни в одном профиле; матрица разрешений — при разборе.
    #[test]
    fn cluster_rpc_requires_internal_mtls() {
        let addr = "  addr: https://[::1]:7443\n";
        let c = parse(&format!("{MINIMAL_DEV}cluster:\n{addr}{RPC_TLS}")).expect("config");
        let cluster = c.section::<ClusterSection>(CLUSTER).expect("cluster");
        assert_eq!(cluster.listen_addr(), Some("[::1]:7443"));
        for missing in ["ca_file", "cert_file", "key_file"] {
            let tls: String = RPC_TLS
                .lines()
                .filter(|l| !l.contains(missing))
                .map(|l| format!("{l}\n"))
                .collect();
            let e = err(parse(&format!("{MINIMAL_DEV}cluster:\n{addr}{tls}")));
            assert!(
                e.contains("rpc.tls") && e.contains(missing),
                "{missing}: {e}"
            );
        }
        // APP-36: ключ по ссылке на секрет вместо файла; оба сразу — ошибка.
        let by_secret = RPC_TLS.replace(
            "key_file: /etc/thepost/node-key.pem",
            "key: { secret: \"thepost/node#key\" }",
        );
        assert_ne!(by_secret, RPC_TLS);
        let c = parse(&format!("{MINIMAL_DEV}cluster:\n{addr}{by_secret}")).expect("key");
        let cluster = c.section::<ClusterSection>(CLUSTER).expect("cluster");
        assert!(cluster.rpc.tls.key.is_some() && cluster.rpc.tls.key_file.is_none());
        let e = err(parse(&format!(
            "{MINIMAL_DEV}cluster:\n{addr}{RPC_TLS}      key: {{ secret: \"thepost/node#key\" }}\n"
        )));
        assert!(e.contains("mutually exclusive"), "{e}");
        // Только сертификат (исходящие запросы без слушателя) — тоже с УЦ.
        let e = err(parse(&format!(
            "{MINIMAL_DEV}cluster:\n  rpc:\n    tls:\n      cert_file: /n.pem\n      key_file: /k.pem\n"
        )));
        assert!(e.contains("ca_file"), "{e}");
        let e = err(parse(&format!(
            "{MINIMAL_DEV}cluster:\n{addr}{RPC_TLS}      allow_plaintext: true\n"
        )));
        assert!(e.contains("allow_plaintext"), "{e}");
        let e = err(parse(&format!(
            "{MINIMAL_DEV}cluster:\n{addr}{RPC_TLS}    permissions:\n      search: [nonsense]\n"
        )));
        assert!(e.contains("nonsense"), "{e}");
        let e = err(parse(&format!(
            "{MINIMAL_DEV}cluster:\n{addr}{RPC_TLS}    max_in_flight: 0\n"
        )));
        assert!(e.contains("max_in_flight"), "{e}");
    }

    #[test]
    fn durations_are_humantime() {
        let c = parse(&format!("{MINIMAL_DEV}doctor:\n  max_clock_skew: 2s\n")).expect("config");
        let doctor = c.section::<DoctorSection>(DOCTOR).expect("doctor");
        assert_eq!(doctor.max_clock_skew, Duration::from_secs(2));
        let e = err(parse(&format!(
            "{MINIMAL_DEV}doctor:\n  max_clock_skew: 5\n"
        )));
        assert!(
            e.contains("duration") || e.contains("max_clock_skew"),
            "{e}"
        );
    }

    /// APP-08: опроса журнала событий нет — ключ `cluster.events_poll_interval` неизвестен (строгая проверка).
    #[test]
    fn events_poll_interval_is_gone() {
        let e = err(strict(&format!(
            "{MINIMAL_DEV}cluster:\n  events_poll_interval: 1s\n"
        )));
        assert!(e.contains("events_poll_interval"), "{e}");
    }

    /// APP-32: период очистки роли maintenance — `1h` по умолчанию, из config.yaml, нулевой — ошибка.
    #[test]
    fn maintenance_purge_interval() {
        let c = parse(MINIMAL_DEV).expect("config");
        let m = c
            .section::<MaintenanceSection>(MAINTENANCE)
            .expect("section");
        assert_eq!(m.purge_interval, Duration::from_secs(3600));
        let c = parse(&format!(
            "{MINIMAL_DEV}maintenance:\n  purge_interval: 10m\n"
        ))
        .expect("config");
        let m = c
            .section::<MaintenanceSection>(MAINTENANCE)
            .expect("section");
        assert_eq!(m.purge_interval, Duration::from_secs(600));
        let e = err(parse(&format!(
            "{MINIMAL_DEV}maintenance:\n  purge_interval: 0s\n"
        )));
        assert!(e.contains("purge_interval"), "{e}");
    }

    /// APP-08: шина — в процессе (по умолчанию) или PostgreSQL LISTEN/NOTIFY; Valkey — ошибка строгой проверки.
    #[test]
    fn bus_kinds() {
        let c = parse(MINIMAL_DEV).expect("config");
        assert_eq!(
            c.section::<BusSection>(BUS).expect("bus").kind,
            BusKind::Local
        );
        let c = strict(&format!("{MINIMAL_DEV}bus:\n  kind: postgres\n")).expect("postgres");
        assert_eq!(
            c.section::<BusSection>(BUS).expect("bus").kind,
            BusKind::Postgres
        );
        let e = err(strict(&format!(
            "{MINIMAL_DEV}bus:\n  kind: valkey\n  valkey:\n    url: rediss://v:6379\n"
        )));
        assert!(e.contains("valkey"), "{e}");
    }

    #[test]
    fn crate_sections_parse_and_url_owns_location() {
        let c = parse(&format!(
            "{MINIMAL_DEV}  local:\n    stale_tmp: 1h\n  s3:\n    request_timeout: 5s\n\
             bus:\n  capacity: 64\n"
        ))
        .expect("config");
        let blob = c.section::<BlobSection>(BLOB).expect("blob");
        let local = blob.local_config(PathBuf::from("/data"));
        assert_eq!(
            (local.root, local.stale_tmp),
            (PathBuf::from("/data"), Duration::from_secs(3600))
        );
        let s3 = blob.s3_config("b".into(), "p".into());
        assert_eq!((s3.bucket.as_str(), s3.prefix.as_str()), ("b", "p"));
        assert_eq!(s3.request_timeout, Duration::from_secs(5));
        let bus = c.section::<BusSection>(BUS).expect("bus");
        assert_eq!(bus.local().capacity, 64);
        // Каталог и бакет — только в blob.url.
        let e = err(parse(&format!("{MINIMAL_DEV}  local:\n    root: /other\n")));
        assert!(e.contains("local.root"), "{e}");
        let e = err(parse(&format!("{MINIMAL_DEV}  s3:\n    bucket: other\n")));
        assert!(e.contains("s3.bucket"), "{e}");
    }

    #[test]
    fn schema_lists_app_sections() {
        let schema = registry().expect("registry").schema_json();
        for key in [
            "\"blob\"",
            "\"bus\"",
            "\"cluster\"",
            "\"database\"",
            "\"meta\"",
            "\"doctor\"",
            "\"verify\"",
            "\"startup\"",
            "\"secrets\"",
            "\"smtp\"",
            "\"sieve\"",
            "\"imap\"",
            "\"jmap\"",
            "\"dav\"",
            "\"search\"",
            "\"admin\"",
            "\"auth\"",
            "\"http\"",
        ] {
            assert!(schema.contains(key), "{key}");
        }
    }

    #[test]
    fn admin_listen_is_rejected_with_hint() {
        for r in [
            parse(&format!("{MINIMAL_DEV}admin:\n  listen: 127.0.0.1:9443\n")),
            strict(&format!("{MINIMAL_DEV}admin:\n  listen: 127.0.0.1:9443\n")),
        ] {
            let m = err(r);
            assert!(m.contains("http.admin.listen"), "{m}");
        }
        let c = parse(&format!(
            "{MINIMAL_DEV}admin:\n  page_size: 50\nhttp:\n  admin: {{ listen: 127.0.0.1:9443, allow_plaintext: true }}\n"
        ))
        .expect("config");
        assert_eq!(
            c.section::<AdminSection>(ADMIN).expect("admin").0.page_size,
            50
        );
        let http = c.section::<HttpSection>(HTTP).expect("http");
        assert_eq!(http.admin.as_ref().map(|l| l.listen.port()), Some(9443));
        assert_eq!(
            *parse(MINIMAL_DEV)
                .expect("config")
                .section::<AdminSection>(ADMIN)
                .expect("admin"),
            AdminSection::default()
        );
        let schema = registry().expect("registry").schema_json();
        assert!(schema.contains("admin.listen не используется"));
    }

    /// `admin.allow_external` — ключ без действия (APP-17): ошибка разбора с подсказкой в обоих режимах.
    #[test]
    fn admin_allow_external_is_rejected_with_hint() {
        for value in ["true", "false"] {
            let yaml = format!("{MINIMAL_DEV}admin:\n  allow_external: {value}\n");
            for r in [parse(&yaml), strict(&yaml)] {
                let m = err(r);
                assert!(m.contains("admin.allow_external"), "{m}");
                assert!(m.contains("http.admin.listen"), "{m}");
            }
        }
        let schema = registry().expect("registry").schema_json();
        assert!(schema.contains("admin.allow_external не используется"));
    }
}
