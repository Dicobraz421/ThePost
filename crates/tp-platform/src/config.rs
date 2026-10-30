//! Загрузка config.yaml (SRS 5.5, CONTRACTS.md#Config): дерево секций крейтов, переопределение `THEPOST_*`,
//! обязательные параметры, роли (SRS 2.1), секции модулей `modules.<имя>` (SRS 2.5), JSON Schema.
//!
//! Центрального перечня секций нет: каждый крейт регистрирует свою секцию в [`SectionRegistry`]
//! (`add_section::<XxxConfig>("xxx")`), приложение собирает реестр и вызывает [`SectionRegistry::load`].
//! Секция разбирается из исходного текста по своему пути, поэтому ошибка содержит путь параметра
//! (`smtp.limits.max_size: invalid type ...`) и позицию в файле.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use schemars::generate::{SchemaGenerator, SchemaSettings};
use schemars::{JsonSchema, Schema, json_schema};
use serde::de::{DeserializeOwned, DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_yaml_ng::{Mapping, Value};
use tp_contracts::Error;
use tp_contracts::role::{RoleEntry, RolePreset, RoleSet};

use crate::reload::ReloadConfig;
use crate::roles::RuntimeConfig;
use crate::service_http::ServiceHttpConfig;
use crate::telemetry::TelemetryConfig;
use crate::tls::TlsConfig;

/// Обязательные параметры (SRS 5.5): адрес PostgreSQL, адрес BLOB-хранилища, основной домен.
pub const REQUIRED: &[&str] = &["database.url", "blob.url", "domain"];

/// Префикс переменных окружения, переопределяющих параметры: `THEPOST_SMTP__LIMITS__MAX_SIZE=10` → `smtp.limits.max_size`.
pub const ENV_PREFIX: &str = "THEPOST_";

/// Переменная с путём к config.yaml; не переопределяет параметр.
pub const ENV_CONFIG_PATH: &str = "THEPOST_CONFIG";

/// Ключи верхнего уровня, которыми владеет платформа.
const CORE_KEYS: &[&str] = &["domain", "roles", "modules"];

/// Секции с обязательным `url`, которые допускаются и без регистрации (их крейты регистрируются приложением).
const URL_SECTIONS: &[&str] = &["database", "blob"];

/// Секция конфигурации крейта (CONTRACTS.md#Config).
pub trait Section: DeserializeOwned + JsonSchema + Default + Send + Sync + 'static {}

impl<T: DeserializeOwned + JsonSchema + Default + Send + Sync + 'static> Section for T {}

/// Значение секции со стёртым типом.
pub type AnySection = Arc<dyn Any + Send + Sync>;

/// Ошибка загрузки конфигурации; на границе крейта — `Error::Invalid`.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}")]
    Parse(#[from] serde_yaml_ng::Error),
    #[error("{0}: required parameter is missing")]
    Missing(String),
    #[error("{0}: unknown parameter")]
    Unknown(String),
    #[error("modules.{0}: module is not built in")]
    NoModule(String),
    #[error("{var}: {path} is not a section")]
    Env { var: String, path: String },
}

impl From<ConfigError> for Error {
    fn from(e: ConfigError) -> Self {
        Error::Invalid(e.to_string())
    }
}

/// Описание зарегистрированной секции.
#[derive(Clone, Copy)]
struct SectionDef {
    schema: fn(&mut SchemaGenerator) -> Schema,
    parse: fn(Option<&str>, &[&str]) -> Result<AnySection, serde_yaml_ng::Error>,
}

/// Реестр секций крейтов и модулей. [`SectionRegistry::new`] уже содержит секции платформы.
#[derive(Clone)]
pub struct SectionRegistry {
    sections: BTreeMap<&'static str, SectionDef>,
    modules: BTreeMap<&'static str, SectionDef>,
}

impl fmt::Debug for SectionRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SectionRegistry")
            .field("sections", &self.sections.keys().collect::<Vec<_>>())
            .field("modules", &self.modules.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for SectionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SectionRegistry {
    /// Реестр с секциями платформы: `telemetry`, `service_http`, `reload`, `runtime`, `tls`.
    pub fn new() -> Self {
        let mut r = Self {
            sections: BTreeMap::new(),
            modules: BTreeMap::new(),
        };
        r.sections.insert("telemetry", def::<TelemetryConfig>());
        r.sections
            .insert("service_http", def::<ServiceHttpConfig>());
        r.sections.insert("reload", def::<ReloadConfig>());
        r.sections.insert("runtime", def::<RuntimeConfig>());
        r.sections.insert("tls", def::<TlsConfig>());
        r
    }

    /// Регистрирует секцию верхнего уровня `name`. Повтор имени — `Error::Conflict`.
    pub fn add_section<T: Section>(&mut self, name: &'static str) -> tp_contracts::Result<()> {
        if CORE_KEYS.contains(&name) || self.sections.contains_key(name) {
            return Err(Error::Conflict);
        }
        self.sections.insert(name, def::<T>());
        Ok(())
    }

    /// Регистрирует модуль `name`: секция `modules.<name>` с `enabled` и полями `T`.
    pub fn add_module<T: Section>(&mut self, name: &'static str) -> tp_contracts::Result<()> {
        if self.modules.contains_key(name) {
            return Err(Error::Conflict);
        }
        self.modules.insert(name, module_def::<T>());
        Ok(())
    }

    /// Читает файл и переменные окружения процесса. Блокирующий I/O: в async — через `spawn_blocking`.
    pub fn load(&self, path: &Path) -> tp_contracts::Result<Config> {
        let src = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        Ok(self.parse_with(&src, std::env::vars())?)
    }

    /// Разбирает текст config.yaml с переопределениями `env` (пары «имя — значение», лишние игнорируются).
    pub fn parse(
        &self,
        src: &str,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> tp_contracts::Result<Config> {
        Ok(self.parse_with(src, env)?)
    }

    pub(crate) fn parse_with(
        &self,
        src: &str,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Config, ConfigError> {
        let mut tree: Value = serde_yaml_ng::from_str(src)?;
        if tree.is_null() {
            tree = Value::Mapping(Mapping::new());
        }
        let overridden = apply_env(&mut tree, env)?;
        // После переопределения позиции в ошибках относятся к объединённому тексту; путь параметра — точный.
        let merged;
        let src = if overridden {
            merged = serde_yaml_ng::to_string(&tree)?;
            merged.as_str()
        } else {
            src
        };

        for path in REQUIRED {
            let key: Vec<&str> = path.split('.').collect();
            match lookup(&tree, &key) {
                Some(Value::String(s)) if !s.trim().is_empty() => {}
                Some(Value::String(_)) | None | Some(Value::Null) => {
                    return Err(ConfigError::Missing((*path).to_owned()));
                }
                Some(_) => {
                    // Не строка: точное сообщение с позицией даст разбор строки.
                    at::<String>(src, &key)?;
                }
            }
        }

        if let Value::Mapping(root) = &tree {
            for key in root.keys() {
                let name = key.as_str().unwrap_or_default();
                let known = CORE_KEYS.contains(&name)
                    || URL_SECTIONS.contains(&name)
                    || self.sections.contains_key(name);
                if !known {
                    return Err(ConfigError::Unknown(yaml_key(key)));
                }
            }
        }

        let domain: String = at(src, &["domain"])?.unwrap_or_default();
        let role_entries: Vec<RoleEntry> =
            at(src, &["roles"])?.unwrap_or_else(|| vec![RoleEntry::Preset(RolePreset::All)]);
        let roles = RoleSet::expand(&role_entries);

        let mut sections = BTreeMap::new();
        for (name, def) in &self.sections {
            let present = lookup(&tree, &[name]).is_some_and(|v| !v.is_null());
            sections.insert(*name, (def.parse)(present.then_some(src), &[name])?);
        }

        let mut modules = BTreeMap::new();
        let listed: Vec<String> = match lookup(&tree, &["modules"]) {
            Some(Value::Mapping(m)) => m.keys().map(yaml_key).collect(),
            _ => Vec::new(),
        };
        for name in &listed {
            if !self.modules.contains_key(name.as_str()) {
                let switch: Option<ModuleSwitch> = at(src, &["modules", name])?;
                if switch.is_some_and(|s| s.enabled) {
                    return Err(ConfigError::NoModule(name.clone()));
                }
            }
        }
        for (name, def) in &self.modules {
            let path = ["modules", name];
            let present = lookup(&tree, &path).is_some_and(|v| !v.is_null());
            let enabled: bool = if present {
                at::<ModuleSwitch>(src, &path)?.is_some_and(|s| s.enabled)
            } else {
                false
            };
            let config = (def.parse)(present.then_some(src), &path)?;
            modules.insert(*name, ModuleState { enabled, config });
        }

        Ok(Config {
            domain,
            role_entries,
            roles,
            sections,
            modules,
        })
    }

    /// JSON Schema config.yaml (draft 2020-12): все зарегистрированные секции и модули.
    pub fn schema(&self) -> Schema {
        let mut generator = SchemaSettings::draft2020_12().into_generator();
        let mut props = BTreeMap::new();
        props.insert("domain".to_owned(), generator.subschema_for::<String>());
        props.insert(
            "roles".to_owned(),
            generator.subschema_for::<Vec<RoleEntry>>(),
        );
        for (name, def) in &self.sections {
            props.insert((*name).to_owned(), (def.schema)(&mut generator));
        }
        for name in URL_SECTIONS {
            let url = json_schema!({ "required": ["url"], "properties": { "url": { "type": "string", "minLength": 1 } } });
            let schema = match props.remove(*name) {
                Some(section) => json_schema!({ "allOf": [section, url] }),
                None => json_schema!({ "type": "object", "allOf": [url] }),
            };
            props.insert((*name).to_owned(), schema);
        }
        let mut modules = BTreeMap::new();
        for (name, def) in &self.modules {
            modules.insert((*name).to_owned(), (def.schema)(&mut generator));
        }
        props.insert(
            "modules".to_owned(),
            json_schema!({
                "type": "object",
                "properties": modules,
                "additionalProperties": generator.subschema_for::<ModuleSwitch>(),
            }),
        );
        let defs = generator.take_definitions(true);
        json_schema!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "The Post config.yaml",
            "type": "object",
            "required": ["database", "blob", "domain"],
            "properties": props,
            "additionalProperties": false,
            "$defs": defs,
        })
    }

    /// Схема в JSON для `thepost config schema`.
    pub fn schema_json(&self) -> String {
        format!("{:#}", self.schema().as_value())
    }
}

/// Загруженная конфигурация узла. Неизменяема; новая версия при hot reload рассылается целиком.
#[derive(Clone)]
pub struct Config {
    domain: String,
    role_entries: Vec<RoleEntry>,
    roles: RoleSet,
    sections: BTreeMap<&'static str, AnySection>,
    modules: BTreeMap<&'static str, ModuleState>,
}

#[derive(Clone)]
struct ModuleState {
    enabled: bool,
    config: AnySection,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("domain", &self.domain)
            .field("roles", &self.role_entries)
            .field("sections", &self.sections.keys().collect::<Vec<_>>())
            .field(
                "modules",
                &self
                    .modules
                    .iter()
                    .map(|(n, m)| (n, m.enabled))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Config {
    /// Основной домен.
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Параметр `roles` как в файле.
    pub fn role_entries(&self) -> &[RoleEntry] {
        &self.role_entries
    }

    /// Роли узла после раскрытия пресетов. По умолчанию — пресет `all`.
    pub fn roles(&self) -> &RoleSet {
        &self.roles
    }

    /// Секция `name` со стёртым типом; `None` — не зарегистрирована.
    pub fn section_any(&self, name: &str) -> Option<AnySection> {
        self.sections.get(name).cloned()
    }

    /// Секция `name` как `T`. Не зарегистрирована — `Error::NotFound`, другой тип — `Error::Internal`.
    pub fn section<T: Any + Send + Sync>(&self, name: &str) -> tp_contracts::Result<Arc<T>> {
        downcast(self.sections.get(name).ok_or(Error::NotFound)?)
    }

    /// Модуль собран и включён (`modules.<name>.enabled: true`).
    pub fn module_enabled(&self, name: &str) -> bool {
        self.modules.get(name).is_some_and(|m| m.enabled)
    }

    /// Секция модуля `name` как `T` (тот же тип, что в `add_module`).
    pub fn module<T: Any + Send + Sync>(&self, name: &str) -> tp_contracts::Result<Arc<T>> {
        downcast(&self.modules.get(name).ok_or(Error::NotFound)?.config)
    }
}

fn downcast<T: Any + Send + Sync>(v: &AnySection) -> tp_contracts::Result<Arc<T>> {
    v.clone()
        .downcast::<T>()
        .map_err(|_| Error::internal("config section type mismatch"))
}

/// Секция `modules.<имя>`: признак включения и поля модуля (неизвестные поля модуля игнорируются из-за `flatten`).
#[derive(Debug, Deserialize, JsonSchema)]
struct ModuleSection<T> {
    /// Модуль включён. По умолчанию выключен.
    #[serde(default)]
    #[allow(dead_code)]
    enabled: bool,
    #[serde(flatten)]
    config: T,
}

/// Признак включения модуля без его полей.
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct ModuleSwitch {
    /// Модуль включён.
    #[serde(default)]
    enabled: bool,
}

fn def<T: Section>() -> SectionDef {
    SectionDef {
        schema: |g| g.subschema_for::<T>(),
        parse: |src, path| {
            let value: Option<T> = match src {
                Some(src) => at(src, path)?,
                None => None,
            };
            Ok(Arc::new(value.unwrap_or_default()))
        },
    }
}

fn module_def<T: Section>() -> SectionDef {
    SectionDef {
        schema: |g| g.subschema_for::<ModuleSection<T>>(),
        parse: |src, path| {
            let value: Option<ModuleSection<T>> = match src {
                Some(src) => at(src, path)?,
                None => None,
            };
            Ok(Arc::new(value.map(|m| m.config).unwrap_or_default()))
        },
    }
}

/// Разбирает из текста значение по пути ключей; `None` — путь отсутствует.
fn at<T: DeserializeOwned>(src: &str, path: &[&str]) -> Result<Option<T>, serde_yaml_ng::Error> {
    At::<T>::new(path).deserialize(serde_yaml_ng::Deserializer::from_str(src))
}

struct At<'p, T> {
    path: &'p [&'p str],
    _t: PhantomData<T>,
}

impl<'p, T> At<'p, T> {
    fn new(path: &'p [&'p str]) -> Self {
        Self {
            path,
            _t: PhantomData,
        }
    }
}

impl<'de, T: Deserialize<'de>> DeserializeSeed<'de> for At<'_, T> {
    type Value = Option<T>;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Option<T>, D::Error> {
        if self.path.is_empty() {
            T::deserialize(d).map(Some)
        } else {
            d.deserialize_any(self)
        }
    }
}

impl<'de, T: Deserialize<'de>> Visitor<'de> for At<'_, T> {
    type Value = Option<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a section containing `{}`", self.path.join("."))
    }

    fn visit_unit<E>(self) -> Result<Option<T>, E> {
        Ok(None)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Option<T>, A::Error> {
        let Some((key, rest)) = self.path.split_first() else {
            return Ok(None);
        };
        let mut found = None;
        while let Some(k) = map.next_key::<KeyStr>()? {
            if found.is_none() && k.0 == *key {
                found = map.next_value_seed(At::<T>::new(rest))?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(found)
    }
}

/// Ключ отображения; не строковые ключи сравниваются как пустая строка.
struct KeyStr(String);

impl<'de> Deserialize<'de> for KeyStr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        Ok(KeyStr(v.as_str().unwrap_or_default().to_owned()))
    }
}

fn lookup<'v>(tree: &'v Value, path: &[&str]) -> Option<&'v Value> {
    path.iter().try_fold(tree, |node, key| node.get(*key))
}

fn yaml_key(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_yaml_ng::to_string(other)
            .map(|s| s.trim_end().to_owned())
            .unwrap_or_default(),
    }
}

/// Применяет `THEPOST_A__B=v` как `a.b: v` (значение — YAML-скаляр или строка). Возвращает, было ли изменение.
fn apply_env(
    tree: &mut Value,
    env: impl IntoIterator<Item = (String, String)>,
) -> Result<bool, ConfigError> {
    let mut vars: Vec<(String, String)> = env
        .into_iter()
        .filter(|(k, _)| k.starts_with(ENV_PREFIX) && k != ENV_CONFIG_PATH)
        .collect();
    vars.sort();
    for (var, raw) in &vars {
        let keys: Vec<String> = var[ENV_PREFIX.len()..]
            .split("__")
            .map(str::to_lowercase)
            .collect();
        if keys.iter().any(String::is_empty) {
            continue;
        }
        let value = serde_yaml_ng::from_str::<Value>(raw).unwrap_or(Value::String(raw.clone()));
        let mut node = &mut *tree;
        for (i, key) in keys.iter().enumerate() {
            if node.is_null() {
                *node = Value::Mapping(Mapping::new());
            }
            let Value::Mapping(map) = node else {
                return Err(ConfigError::Env {
                    var: var.clone(),
                    path: keys[..i].join("."),
                });
            };
            node = map.entry(Value::String(key.clone())).or_insert(Value::Null);
        }
        *node = value;
    }
    Ok(!vars.is_empty())
}

#[cfg(test)]
mod tests {
    use tp_contracts::role::Role;

    use super::*;

    const MINIMAL: &str = "database: { url: postgres://db/thepost }\nblob: { url: file:///var/lib/thepost }\ndomain: example.com\n";

    #[derive(Debug, Default, Deserialize, JsonSchema, PartialEq)]
    #[serde(default, deny_unknown_fields)]
    struct Limits {
        max_size: u64,
        burst: u32,
    }

    #[derive(Debug, Default, Deserialize, JsonSchema, PartialEq)]
    #[serde(default, deny_unknown_fields)]
    struct SmtpConfig {
        banner: String,
        limits: Limits,
    }

    #[derive(Debug, Default, Deserialize, JsonSchema, PartialEq)]
    #[serde(default)]
    struct ArchiveConfig {
        days: u32,
    }

    fn registry() -> SectionRegistry {
        let mut r = SectionRegistry::new();
        r.add_section::<SmtpConfig>("smtp").expect("smtp");
        r.add_module::<ArchiveConfig>("archive").expect("archive");
        r
    }

    fn parse(src: &str) -> Result<Config, ConfigError> {
        registry().parse_with(src, Vec::new())
    }

    fn err(src: &str) -> String {
        parse(src).expect_err("must fail").to_string()
    }

    #[test]
    fn minimal_three_lines() {
        assert_eq!(MINIMAL.lines().count(), 3);
        let cfg = parse(MINIMAL).expect("minimal");
        assert_eq!(cfg.domain(), "example.com");
        assert_eq!(cfg.roles().roles.len(), Role::ALL.len());
        assert_eq!(
            *cfg.section::<SmtpConfig>("smtp").expect("smtp"),
            SmtpConfig::default()
        );
        assert_eq!(
            cfg.section::<TelemetryConfig>("telemetry")
                .expect("telemetry")
                .log_level,
            TelemetryConfig::default().log_level
        );
        assert!(!cfg.module_enabled("archive"));
        assert!(cfg.section::<SmtpConfig>("nope").is_err());
        assert!(cfg.section::<Limits>("smtp").is_err());
    }

    #[test]
    fn required_parameters() {
        assert_eq!(
            err("blob: { url: x }\ndomain: d"),
            "database.url: required parameter is missing"
        );
        assert_eq!(
            err("database: { url: x }\nblob: {}\ndomain: d"),
            "blob.url: required parameter is missing"
        );
        assert_eq!(
            err("database: { url: x }\nblob: { url: y }\ndomain: ''"),
            "domain: required parameter is missing"
        );
        assert_eq!(err(""), "database.url: required parameter is missing");
    }

    #[test]
    fn error_names_parameter_path() {
        let e = err(&format!("{MINIMAL}smtp:\n  limits:\n    max_size: big\n"));
        assert!(e.starts_with("smtp.limits.max_size: invalid type"), "{e}");
        assert!(e.contains("line 6"), "{e}");
        let e = err(&format!("{MINIMAL}smtp:\n  limits:\n    bogus: 1\n"));
        assert!(e.starts_with("smtp.limits: unknown field `bogus`"), "{e}");
        let e = err(&format!("{MINIMAL}roles: [mailbox, imapp]\n"));
        // `RoleEntry` — untagged enum: путь до параметра, без индекса элемента.
        assert!(e.starts_with("roles: ") && e.contains("line 4"), "{e}");
        assert_eq!(
            err(&format!("{MINIMAL}smtpp: {{}}\n")),
            "smtpp: unknown parameter"
        );
    }

    #[test]
    fn roles_and_modules() {
        let src = format!(
            "{MINIMAL}roles: [backend, imap]\nmodules:\n  archive: {{ enabled: true, days: 30 }}\n  absent: {{ enabled: false }}\n"
        );
        let cfg = parse(&src).expect("parse");
        let roles: Vec<Role> = cfg.roles().roles.iter().copied().collect();
        assert_eq!(
            roles,
            [Role::Imap, Role::Search, Role::Indexer, Role::Maintenance]
        );
        assert!(!cfg.roles().edge);
        assert!(cfg.module_enabled("archive"));
        assert_eq!(cfg.module::<ArchiveConfig>("archive").expect("m").days, 30);
        assert_eq!(
            err(&format!(
                "{MINIMAL}modules:\n  absent: {{ enabled: true }}\n"
            )),
            "modules.absent: module is not built in"
        );
        assert!(
            parse(&format!("{MINIMAL}roles: [edge]\n"))
                .expect("edge")
                .roles()
                .edge
        );
    }

    #[test]
    fn env_overrides() {
        let env = [
            ("THEPOST_DOMAIN", "corp.example"),
            ("THEPOST_SMTP__LIMITS__MAX_SIZE", "1024"),
            ("THEPOST_ROLES", "[access]"),
            ("THEPOST_CONFIG", "/etc/thepost/config.yaml"),
            ("HOME", "/root"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let cfg = registry().parse_with(MINIMAL, env).expect("env");
        assert_eq!(cfg.domain(), "corp.example");
        assert_eq!(
            cfg.section::<SmtpConfig>("smtp")
                .expect("smtp")
                .limits
                .max_size,
            1024
        );
        assert!(cfg.roles().contains(Role::Jmap));
        assert!(!cfg.roles().contains(Role::Search));

        let env = [("THEPOST_DOMAIN__X".to_owned(), "1".to_owned())];
        let e = registry()
            .parse_with(MINIMAL, env)
            .expect_err("scalar")
            .to_string();
        assert_eq!(e, "THEPOST_DOMAIN__X: domain is not a section");
        // Обязательные параметры можно задать только окружением.
        let env = [
            ("THEPOST_DATABASE__URL", "postgres://db"),
            ("THEPOST_BLOB__URL", "s3://b"),
            ("THEPOST_DOMAIN", "d"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        registry().parse_with("", env).expect("env only");
    }

    #[test]
    fn duplicate_registration() {
        let mut r = registry();
        assert_eq!(r.add_section::<SmtpConfig>("smtp"), Err(Error::Conflict));
        assert_eq!(r.add_section::<SmtpConfig>("roles"), Err(Error::Conflict));
        assert_eq!(
            r.add_module::<ArchiveConfig>("archive"),
            Err(Error::Conflict)
        );
    }

    #[test]
    fn schema_covers_all_sections() {
        let r = registry();
        let schema = r.schema();
        let v = schema.as_value();
        let props = v["properties"].as_object().expect("properties");
        for key in CORE_KEYS
            .iter()
            .chain(URL_SECTIONS)
            .chain(r.sections.keys())
        {
            assert!(props.contains_key(*key), "{key}");
        }
        assert!(v["properties"]["modules"]["properties"]["archive"].is_object());
        assert_eq!(v["additionalProperties"], false);
        assert_eq!(v["required"][0], "database");
        let json = r.schema_json();
        assert!(json.contains("\"$defs\""));
        assert!(json.contains("SmtpConfig"));
        assert!(json.contains("max_size"));
    }
}
