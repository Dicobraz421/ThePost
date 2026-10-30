//! Секции config.yaml приложения и реестр секций всех крейтов (SRS 5.5; APP-01, PLT-09).
//!
//! Приложение регистрирует секции полос (`database`, `meta`) и свои: `blob` — хранилище BLOB (локальный каталог
//! или S3), `bus` — шина уведомлений (в процессе или Valkey), `cluster` — имя узла и опросы, `doctor` — пороги
//! проверок окружения. Проверки открытых каналов подключаются через [`SectionRegistry::add_check`]: `thepost config
//! check` (строгий режим) и запуск (мягкий режим) отвергают адрес без TLS вне профиля dev.
//!
//! Конфигурации `tp-blob` и `tp-cluster` пока без `Deserialize` и с длительностями в секундах/миллисекундах;
//! здесь — их секции с длительностями humantime, переводимые в конфигурации крейтов.

use std::path::PathBuf;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tp_blob::{LocalConfig, S3Config};
use tp_cluster::{LocalBusConfig, ValkeyBusConfig};
use tp_contracts::config::duration;
use tp_contracts::secrets::SecretRef;
use tp_contracts::tls::{InternalChannel, InternalTls, Profile};
use tp_contracts::{Error, Result};
use tp_meta::MetaConfig;
use tp_meta::pool::{PoolConfig, url_has_password};
use tp_platform::SectionRegistry;

use crate::modules;

/// Имя секции хранилища BLOB.
pub const BLOB: &str = "blob";
/// Имя секции шины уведомлений.
pub const BUS: &str = "bus";
/// Имя секции кластера.
pub const CLUSTER: &str = "cluster";
/// Имя секции порогов `thepost doctor`.
pub const DOCTOR: &str = "doctor";

/// Реестр секций: платформа, `tp-meta`, приложение и встроенные модули; проверки каналов.
pub fn registry() -> Result<SectionRegistry> {
    let mut r = SectionRegistry::new();
    r.add_section::<PoolConfig>(PoolConfig::SECTION)?;
    r.add_section::<MetaConfig>(MetaConfig::SECTION)?;
    r.add_section::<BlobSection>(BLOB)?;
    r.add_section::<BusSection>(BUS)?;
    r.add_section::<ClusterSection>(CLUSTER)?;
    r.add_section::<DoctorSection>(DOCTOR)?;
    r.add_check::<PoolConfig>(PoolConfig::SECTION, |c, profile| {
        pool_config(c, profile).check().map(drop)
    })?;
    r.add_check::<BlobSection>(BLOB, BlobSection::check)?;
    r.add_check::<BusSection>(BUS, BusSection::check)?;
    r.add_check::<ClusterSection>(CLUSTER, |c, _| c.check())?;
    modules::register_sections(&mut r)?;
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
///     access_key_id: { secret: "thepost/s3#access_key_id" }
///     secret_access_key: { secret: "thepost/s3#secret_access_key" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BlobSection {
    /// Адрес хранилища: `file:///<каталог>` — локальный каталог (в том числе общий для узлов),
    /// `s3://<бакет>[/<префикс>]` — S3 (адрес API — `s3.endpoint`). Требует перезапуска.
    pub url: String,
    /// Параметры локального каталога.
    pub local: LocalSection,
    /// Параметры S3.
    pub s3: S3Section,
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

    /// Проверка секции: адрес, TLS канала S3 (`https://`; `http://` — только dev с `s3.tls.allow_plaintext`).
    pub fn check(&self, profile: Profile) -> Result<()> {
        self.s3.tls.validate(InternalChannel::S3, profile)?;
        match self.location()? {
            BlobLocation::Local(_) => Ok(()),
            BlobLocation::S3 { bucket, prefix } => self
                .s3
                .config(bucket, prefix, None)
                .check(profile)
                .map(drop),
        }
    }
}

/// Подсекция `blob.local`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LocalSection {
    /// fsync файла и каталога при записи (выключать нельзя для источника истины).
    pub fsync: bool,
    /// Размер блока потокового чтения, байт.
    pub read_chunk_bytes: usize,
    /// Возраст брошенного временного файла, после которого он удаляется при открытии (`24h`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub stale_tmp: Duration,
}

impl Default for LocalSection {
    fn default() -> Self {
        let d = LocalConfig::default();
        Self {
            fsync: d.fsync,
            read_chunk_bytes: d.read_chunk_bytes,
            stale_tmp: Duration::from_secs(d.stale_tmp_secs),
        }
    }
}

impl LocalSection {
    /// Конфигурация `tp-blob` для каталога `root`.
    pub fn config(&self, root: PathBuf) -> LocalConfig {
        LocalConfig {
            root,
            fsync: self.fsync,
            read_chunk_bytes: self.read_chunk_bytes,
            stale_tmp_secs: self.stale_tmp.as_secs(),
        }
    }
}

/// Подсекция `blob.s3`. Ключи доступа — только ссылки на секреты; не заданы — из окружения или роли экземпляра.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct S3Section {
    /// Адрес S3 API (`https://seaweedfs:8333`); не задан — AWS по региону.
    pub endpoint: Option<String>,
    /// Регион.
    pub region: String,
    /// Адресация path-style (SeaweedFS, MinIO).
    pub path_style: bool,
    /// Идентификатор ключа доступа — ссылка на секрет.
    pub access_key_id: Option<SecretRef>,
    /// Секрет ключа доступа — ссылка на секрет.
    pub secret_access_key: Option<SecretRef>,
    /// TLS канала до S3.
    pub tls: InternalTls,
    /// Объекты не меньше этого размера загружаются multipart, байт.
    pub multipart_threshold_bytes: u64,
    /// Размер части multipart, байт.
    pub multipart_part_bytes: usize,
    /// Одновременно загружаемых частей объекта.
    pub multipart_concurrency: usize,
    /// Повторов запроса при временных сбоях.
    pub max_retries: usize,
    /// Предел времени повторов запроса.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry_timeout: Duration,
    /// Таймаут запроса.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub request_timeout: Duration,
    /// Таймаут установления соединения.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub connect_timeout: Duration,
}

impl Default for S3Section {
    fn default() -> Self {
        let d = S3Config::default();
        Self {
            endpoint: d.endpoint,
            region: d.region,
            path_style: d.path_style,
            access_key_id: None,
            secret_access_key: None,
            tls: d.tls,
            multipart_threshold_bytes: d.multipart_threshold_bytes,
            multipart_part_bytes: d.multipart_part_bytes,
            multipart_concurrency: d.multipart_concurrency,
            max_retries: d.max_retries,
            retry_timeout: Duration::from_millis(d.retry_timeout_ms),
            request_timeout: Duration::from_millis(d.request_timeout_ms),
            connect_timeout: Duration::from_millis(d.connect_timeout_ms),
        }
    }
}

impl S3Section {
    /// Конфигурация `tp-blob` для бакета и префикса; `keys` — значения ключей доступа из хранилища секретов.
    pub fn config(
        &self,
        bucket: String,
        prefix: String,
        keys: Option<(String, String)>,
    ) -> S3Config {
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        let (access_key_id, secret_access_key) = keys.unzip();
        S3Config {
            endpoint: self.endpoint.clone(),
            bucket,
            region: self.region.clone(),
            prefix,
            access_key_id,
            secret_access_key,
            path_style: self.path_style,
            tls: self.tls.clone(),
            multipart_threshold_bytes: self.multipart_threshold_bytes,
            multipart_part_bytes: self.multipart_part_bytes,
            multipart_concurrency: self.multipart_concurrency,
            max_retries: self.max_retries,
            retry_timeout_ms: ms(self.retry_timeout),
            request_timeout_ms: ms(self.request_timeout),
            connect_timeout_ms: ms(self.connect_timeout),
            ..S3Config::default()
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
    /// Valkey pub/sub: несколько узлов (Enterprise).
    Valkey,
}

/// Секция `bus`: шина уведомлений между узлами (SRS 2.1). Требует перезапуска.
///
/// ```yaml
/// bus:
///   kind: valkey
///   valkey:
///     url: rediss://valkey.internal:6379
///     password: { secret: "thepost/valkey#password" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BusSection {
    /// Реализация: `local` — в процессе, `valkey` — Valkey.
    pub kind: BusKind,
    /// Буфер подписчика шины в процессе: столько непрочитанных уведомлений до отключения отставшего.
    pub capacity: usize,
    /// Параметры Valkey (`kind: valkey`).
    pub valkey: ValkeySection,
}

impl Default for BusSection {
    fn default() -> Self {
        Self {
            kind: BusKind::Local,
            capacity: LocalBusConfig::default().capacity,
            valkey: ValkeySection::default(),
        }
    }
}

impl BusSection {
    /// Проверка: адрес Valkey `rediss://` (`redis://` — только dev с `valkey.tls.allow_plaintext`),
    /// пароль — только ссылкой `valkey.password` (в адресе — только dev).
    pub fn check(&self, profile: Profile) -> Result<()> {
        self.valkey.tls.validate(InternalChannel::Valkey, profile)?;
        if self.kind != BusKind::Valkey {
            return Ok(());
        }
        if profile != Profile::Dev && url_has_password(&self.valkey.url) {
            return Err(Error::invalid(
                "bus.valkey.url: пароль в адресе допустим только в профиле dev; \
                 задайте bus.valkey.password: { secret: \"<путь>#<поле>\" }",
            ));
        }
        self.valkey.config(None).check(profile).map(drop)
    }

    /// Конфигурация шины в процессе.
    pub fn local(&self) -> LocalBusConfig {
        LocalBusConfig {
            capacity: self.capacity,
        }
    }
}

/// Подсекция `bus.valkey`.
#[derive(Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ValkeySection {
    /// Адрес без пароля: `rediss://узел:порт[/база]`.
    pub url: String,
    /// Пароль — ссылка на секрет.
    pub password: Option<SecretRef>,
    /// TLS канала: CA, клиентский сертификат (mTLS), `allow_plaintext` (dev).
    pub tls: InternalTls,
    /// Префикс имён каналов.
    pub channel_prefix: String,
    /// Буфер уведомлений подписчика на узле.
    pub buffer: usize,
    /// Таймаут установки соединения.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub connect_timeout: Duration,
    /// Таймаут команды.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub command_timeout: Duration,
    /// Начальная задержка переподключения (удваивается до `reconnect_max`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub reconnect_min: Duration,
    /// Предельная задержка переподключения.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub reconnect_max: Duration,
    /// Период проверки соединения PING.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub ping_interval: Duration,
}

impl std::fmt::Debug for ValkeySection {
    // Адрес в dev может содержать пароль — в журнал не попадает.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValkeySection")
            .field("password", &self.password)
            .field("tls", &self.tls)
            .field("channel_prefix", &self.channel_prefix)
            .finish_non_exhaustive()
    }
}

impl Default for ValkeySection {
    fn default() -> Self {
        let d = ValkeyBusConfig::default();
        Self {
            url: d.url,
            password: None,
            tls: d.tls,
            channel_prefix: d.channel_prefix,
            buffer: d.buffer,
            connect_timeout: Duration::from_millis(d.connect_timeout_ms),
            command_timeout: Duration::from_millis(d.command_timeout_ms),
            reconnect_min: Duration::from_millis(d.reconnect_min_ms),
            reconnect_max: Duration::from_millis(d.reconnect_max_ms),
            ping_interval: Duration::from_millis(d.ping_interval_ms),
        }
    }
}

impl ValkeySection {
    /// Конфигурация `tp-cluster`; `password` — значение из хранилища секретов (вставляется в адрес).
    pub fn config(&self, password: Option<&str>) -> ValkeyBusConfig {
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        ValkeyBusConfig {
            url: match password {
                Some(p) => url_with_password(&self.url, p),
                None => self.url.clone(),
            },
            tls: self.tls.clone(),
            channel_prefix: self.channel_prefix.clone(),
            buffer: self.buffer,
            connect_timeout_ms: ms(self.connect_timeout),
            command_timeout_ms: ms(self.command_timeout),
            reconnect_min_ms: ms(self.reconnect_min),
            reconnect_max_ms: ms(self.reconnect_max),
            ping_interval_ms: ms(self.ping_interval),
        }
    }
}

/// Адрес `scheme://[user@]host…` с паролем `scheme://[user]:<пароль>@host…` (пароль кодируется `%XX`).
pub fn url_with_password(url: &str, password: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let (user, host) = match authority.rsplit_once('@') {
        Some((userinfo, host)) => (userinfo.split(':').next().unwrap_or_default(), host),
        None => ("", authority),
    };
    let mut encoded = String::with_capacity(password.len());
    for b in password.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            encoded.push(char::from(b));
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("{scheme}://{user}:{encoded}@{host}{tail}")
}

// ---------- cluster ----------

/// Секция `cluster`: узел в составе кластера (членство, разделы журнала событий).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ClusterSection {
    /// Имя узла в членстве кластера, постоянное для узла; не задано — имя хоста. Требует перезапуска.
    pub node_id: Option<String>,
    /// Период опроса живых узлов для назначения разделов журнала (не больше периода продления записи узла).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub assign_poll_interval: Duration,
    /// Период опроса журнала событий подписчиками узла.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub events_poll_interval: Duration,
}

impl Default for ClusterSection {
    fn default() -> Self {
        Self {
            node_id: None,
            assign_poll_interval: Duration::from_secs(2),
            events_poll_interval: Duration::from_secs(1),
        }
    }
}

impl ClusterSection {
    /// Проверка: имя узла (если задано) и ненулевые периоды.
    pub fn check(&self) -> Result<()> {
        if let Some(node) = &self.node_id {
            tp_contracts::membership::NodeId::new(node.clone())?;
        }
        if self.assign_poll_interval.is_zero() || self.events_poll_interval.is_zero() {
            return Err(Error::invalid("периоды опроса должны быть больше нуля"));
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
}

impl Default for DoctorSection {
    fn default() -> Self {
        Self {
            warn_clock_skew: Duration::from_millis(500),
            max_clock_skew: Duration::from_secs(5),
            blob_timeout: Duration::from_secs(10),
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
        // Valkey без TLS и с паролем в адресе.
        let e = err(parse(&prod(
            "bus:\n  kind: valkey\n  valkey:\n    url: redis://v:6379\n",
        )));
        assert!(e.contains("bus"), "{e}");
        let e = err(parse(&prod(
            "bus:\n  kind: valkey\n  valkey:\n    url: rediss://:pw@v:6379\n",
        )));
        assert!(e.contains("bus.valkey.url"), "{e}");
        assert!(!e.contains("pw@"), "{e}");
        // allow_plaintext вне dev — ошибка даже у невыбранной реализации.
        let e = err(parse(&prod(
            "bus:\n  valkey:\n    tls: { allow_plaintext: true }\n",
        )));
        assert!(e.contains("allow_plaintext"), "{e}");
        // dev с явным разрешением — можно.
        parse(&format!(
            "{MINIMAL_DEV}bus:\n  kind: valkey\n  valkey:\n    url: redis://v:6379\n    tls: {{ allow_plaintext: true }}\n"
        ))
        .expect("dev plaintext");
    }

    #[test]
    fn secrets_only_by_reference() {
        let e = err(parse(&format!(
            "{MINIMAL_DEV}  s3:\n    secret_access_key: hunter2\n"
        )));
        assert!(!e.contains("hunter2"), "{e}");
        assert!(e.contains("secret"), "{e}");
        let c = parse(&format!(
            "{MINIMAL_DEV}  s3:\n    secret_access_key: {{ secret: \"thepost/s3#key\" }}\n"
        ))
        .expect("reference");
        let blob = c.section::<BlobSection>(BLOB).expect("blob");
        assert_eq!(
            blob.s3.secret_access_key.as_ref().map(ToString::to_string),
            Some("thepost/s3#key".to_owned())
        );
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

    #[test]
    fn durations_are_humantime() {
        let c = parse(&format!(
            "{MINIMAL_DEV}cluster:\n  assign_poll_interval: 500ms\ndoctor:\n  max_clock_skew: 2s\n"
        ))
        .expect("config");
        let cluster = c.section::<ClusterSection>(CLUSTER).expect("cluster");
        assert_eq!(cluster.assign_poll_interval, Duration::from_millis(500));
        let e = err(parse(&format!(
            "{MINIMAL_DEV}cluster:\n  assign_poll_interval: 5\n"
        )));
        assert!(
            e.contains("duration") || e.contains("assign_poll_interval"),
            "{e}"
        );
    }

    #[test]
    fn valkey_password_goes_into_url() {
        assert_eq!(
            url_with_password("rediss://v:6379/0", "p@ss:w/rd"),
            "rediss://:p%40ss%3Aw%2Frd@v:6379/0"
        );
        assert_eq!(
            url_with_password("rediss://user@v:6379", "pw"),
            "rediss://user:pw@v:6379"
        );
        let s = ValkeySection {
            url: "rediss://v:6379".into(),
            ..ValkeySection::default()
        };
        assert_eq!(s.config(Some("x")).url, "rediss://:x@v:6379");
        assert!(!format!("{s:?}").contains("6379"));
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
        ] {
            assert!(schema.contains(key), "{key}");
        }
    }
}
