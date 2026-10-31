//! Секции config.yaml приложения и реестр секций всех крейтов (SRS 5.5; APP-01, PLT-09).
//!
//! Приложение регистрирует секции полос (`database`, `meta`) и свои: `blob` — хранилище BLOB (локальный каталог
//! или S3), `bus` — шина уведомлений (в процессе или Valkey), `cluster` — имя узла и опросы, `doctor` — пороги
//! проверок окружения. Проверки открытых каналов подключаются через [`SectionRegistry::add_check`]: `thepost config
//! check` (строгий режим) и запуск (мягкий режим) отвергают адрес без TLS вне профиля dev.
//!
//! Подсекции `blob.local`, `blob.s3` и `bus.valkey` — секции крейтов `tp-blob` и `tp-cluster` как есть (BLOB-11,
//! CLU-07): приложение подставляет только каталог, бакет и префикс из `blob.url`.

use std::path::PathBuf;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tp_blob::{GcConfig, LocalConfig, OrphanConfig, S3Config};
use tp_cluster::{LocalBusConfig, ValkeyConfig};
use tp_contracts::config::duration;
use tp_contracts::tls::{InternalChannel, Profile};
use tp_contracts::{Error, Result};
use tp_meta::MetaConfig;
use tp_meta::pool::PoolConfig;
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
/// Имя секции `thepost verify`.
pub const VERIFY: &str = "verify";

/// Реестр секций: платформа, `tp-meta`, приложение и встроенные модули; проверки каналов.
pub fn registry() -> Result<SectionRegistry> {
    let mut r = SectionRegistry::new();
    r.add_section::<PoolConfig>(PoolConfig::SECTION)?;
    r.add_section::<MetaConfig>(MetaConfig::SECTION)?;
    r.add_section::<BlobSection>(BLOB)?;
    r.add_section::<BusSection>(BUS)?;
    r.add_section::<ClusterSection>(CLUSTER)?;
    r.add_section::<DoctorSection>(DOCTOR)?;
    r.add_section::<VerifySection>(VERIFY)?;
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
    /// Параметры Valkey (`kind: valkey`) — секция `tp-cluster`.
    pub valkey: ValkeyConfig,
}

impl Default for BusSection {
    fn default() -> Self {
        Self {
            kind: BusKind::Local,
            capacity: LocalBusConfig::default().capacity,
            valkey: ValkeyConfig::default(),
        }
    }
}

impl BusSection {
    /// Проверка: адрес Valkey `rediss://` (`redis://` — только dev с `valkey.tls.allow_plaintext`),
    /// пароль — только ссылкой `valkey.password` (в адресе — только dev): `ValkeyConfig::check`.
    pub fn check(&self, profile: Profile) -> Result<()> {
        self.valkey.tls.validate(InternalChannel::Valkey, profile)?;
        if self.kind != BusKind::Valkey {
            return Ok(());
        }
        self.valkey.check(profile).map(drop)
    }

    /// Конфигурация шины в процессе.
    pub fn local(&self) -> LocalBusConfig {
        LocalBusConfig {
            capacity: self.capacity,
        }
    }
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
        assert!(e.contains("bus") && e.contains("пароль в адресе"), "{e}");
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
    fn crate_sections_parse_and_url_owns_location() {
        let c = parse(&format!(
            "{MINIMAL_DEV}  local:\n    stale_tmp: 1h\n  s3:\n    request_timeout: 5s\n\
             bus:\n  valkey:\n    url: rediss://v:6379\n    connect_timeout: 2s\n"
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
        assert_eq!(bus.valkey.connect_timeout, Duration::from_secs(2));
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
            "\"secrets\"",
        ] {
            assert!(schema.contains(key), "{key}");
        }
    }
}
