//! Загрузка config.yaml (SRS 5.5, CONTRACTS.md#Config): дерево секций крейтов, переопределение `THEPOST_*`,
//! обязательные параметры, роли (SRS 2.1), секции модулей `modules.<имя>` (SRS 2.5), JSON Schema.
//!
//! Центрального перечня секций нет: каждый крейт регистрирует свою секцию в [`SectionRegistry`]
//! (`add_section::<XxxConfig>("xxx")`), приложение собирает реестр и вызывает [`SectionRegistry::load`].
//! Секция разбирается из исходного текста по своему пути, поэтому ошибка содержит путь параметра
//! (`smtp.limits.max_size: invalid type ...`) и позицию в файле.
//!
//! Два режима проверки (решение #29, PLT-09): строгий ([`CheckMode::Strict`], `thepost config check`) и мягкий
//! ([`CheckMode::Lenient`], старт и hot reload). Оба собирают все ошибки разом: отсутствующие обязательные параметры,
//! неверные типы (первая в каждой секции), открытые внутренние каналы вне профиля dev. Неизвестные параметры
//! (по JSON Schema секций, включая секции модулей) в строгом режиме — ошибки, в мягком — предупреждения в лог
//! ([`Config::warnings`]); в обоих режимах они отбрасываются до разбора секций.
//!
//! Секреты (решение 2026-10-30, PLT-12) — только ссылки `{ secret: "<путь>#<поле>" }`: значение открытым текстом
//! в параметре с именем секрета (`password`, `token`, `secret_access_key` …) и пароль в адресе вне профиля dev —
//! ошибки обоих режимов. Ссылки всех секций собираются при разборе ([`Config::secret_refs`]) и читаются из
//! хранилища [`Config::resolve_secrets`] при запуске и перечитывании.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
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
use tp_contracts::secrets::{Secret, SecretLease, SecretRef, SecretStore};
use tp_contracts::tls::Profile;

use crate::acme::AcmeConfig;
use crate::ratelimit::RateLimitConfig;
use crate::reload::ReloadConfig;
use crate::roles::RuntimeConfig;
use crate::secrets::SecretsConfig;
use crate::service_http::ServiceHttpConfig;
use crate::telemetry::TelemetryConfig;
use crate::tls::TlsConfig;

/// Обязательные параметры (SRS 5.5): адрес PostgreSQL, адрес BLOB-хранилища, основной домен.
pub const REQUIRED: &[&str] = &["database.url", "blob.url", "domain"];

/// Префикс переменных окружения, переопределяющих параметры: `THEPOST_SMTP__LIMITS__MAX_SIZE=10` → `smtp.limits.max_size`.
pub const ENV_PREFIX: &str = "THEPOST_";

/// Переменная с путём к config.yaml; не переопределяет параметр.
pub const ENV_CONFIG_PATH: &str = "THEPOST_CONFIG";

/// Имена параметров со значением-секретом: допустима только ссылка `{ secret: "<путь>#<поле>" }` (регистр не важен).
pub const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "secret_access_key",
    "secret_key",
    "client_secret",
    "secret_id",
    "api_key",
    "private_key",
    "token",
    "access_token",
    "refresh_token",
];

/// Сообщение о секрете открытым текстом (без значения).
const PLAINTEXT_SECRET: &str =
    "secret in plaintext is not allowed; use a reference { secret: \"<path>#<field>\" }";

/// Ключи верхнего уровня, которыми владеет платформа.
const CORE_KEYS: &[&str] = &["domain", "profile", "roles", "modules"];

/// Секции с обязательным `url`, которые допускаются и без регистрации (их крейты регистрируются приложением).
const URL_SECTIONS: &[&str] = &["database", "blob"];

/// Секция конфигурации крейта (CONTRACTS.md#Config).
pub trait Section: DeserializeOwned + JsonSchema + Default + Send + Sync + 'static {}

impl<T: DeserializeOwned + JsonSchema + Default + Send + Sync + 'static> Section for T {}

/// Значение секции со стёртым типом.
pub type AnySection = Arc<dyn Any + Send + Sync>;

/// Режим проверки конфигурации.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckMode {
    /// `thepost config check`: неизвестные параметры — ошибки.
    Strict,
    /// Старт и hot reload: неизвестные параметры — предупреждения в лог.
    #[default]
    Lenient,
}

/// Проверка разобранной секции с учётом профиля (адреса внутренних каналов и т. п.).
type CheckFn = Arc<dyn Fn(&AnySection, Profile) -> tp_contracts::Result<()> + Send + Sync>;

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
    /// Все найденные ошибки, по одной в строке.
    #[error("{}", .0.join("\n"))]
    Invalid(Vec<String>),
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
    checks: BTreeMap<&'static str, Vec<CheckFn>>,
}

impl fmt::Debug for SectionRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SectionRegistry")
            .field("sections", &self.sections.keys().collect::<Vec<_>>())
            .field("modules", &self.modules.keys().collect::<Vec<_>>())
            .field("checks", &self.checks.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for SectionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SectionRegistry {
    /// Реестр с секциями платформы: `telemetry`, `service_http`, `reload`, `runtime`, `tls`, `ratelimit`, `acme`,
    /// `secrets`.
    pub fn new() -> Self {
        let mut r = Self {
            sections: BTreeMap::new(),
            modules: BTreeMap::new(),
            checks: BTreeMap::new(),
        };
        r.sections.insert("telemetry", def::<TelemetryConfig>());
        r.sections
            .insert("service_http", def::<ServiceHttpConfig>());
        r.sections.insert("reload", def::<ReloadConfig>());
        r.sections.insert("runtime", def::<RuntimeConfig>());
        r.sections.insert("tls", def::<TlsConfig>());
        r.sections.insert("ratelimit", def::<RateLimitConfig>());
        r.sections.insert("acme", def::<AcmeConfig>());
        r.sections.insert("secrets", def::<SecretsConfig>());
        r.checks
            .entry("service_http")
            .or_default()
            .push(check_fn(ServiceHttpConfig::check));
        r.checks
            .entry("secrets")
            .or_default()
            .push(check_fn(SecretsConfig::check));
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

    /// Регистрирует проверку секции `name` (тип `T` — как в `add_section`): адреса внутренних каналов и прочие
    /// условия, зависящие от профиля. Выполняется в обоих режимах; ошибка — ошибка конфигурации. Секции с проверкой
    /// не просматриваются общим поиском `tls.allow_plaintext`. Секция не зарегистрирована — `Error::NotFound`.
    pub fn add_check<T: Section>(
        &mut self,
        name: &'static str,
        check: impl Fn(&T, Profile) -> tp_contracts::Result<()> + Send + Sync + 'static,
    ) -> tp_contracts::Result<()> {
        if !self.sections.contains_key(name) {
            return Err(Error::NotFound);
        }
        self.checks.entry(name).or_default().push(check_fn(check));
        Ok(())
    }

    /// Читает файл и переменные окружения процесса в мягком режиме (старт сервера); предупреждения — в лог.
    /// Блокирующий I/O: в async — через `spawn_blocking`.
    pub fn load(&self, path: &Path) -> tp_contracts::Result<Config> {
        self.load_mode(path, CheckMode::Lenient)
    }

    /// Строгая проверка файла для `thepost config check`: все ошибки разом, неизвестные параметры — ошибки.
    /// Блокирующий I/O.
    pub fn check(&self, path: &Path) -> tp_contracts::Result<Config> {
        self.load_mode(path, CheckMode::Strict)
    }

    fn load_mode(&self, path: &Path, mode: CheckMode) -> tp_contracts::Result<Config> {
        let src = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        Ok(self.parse_with(&src, std::env::vars(), mode)?)
    }

    /// Разбирает текст config.yaml в мягком режиме с переопределениями `env` (пары «имя — значение»,
    /// лишние игнорируются); предупреждения — в лог и в [`Config::warnings`].
    pub fn parse(
        &self,
        src: &str,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> tp_contracts::Result<Config> {
        Ok(self.parse_with(src, env, CheckMode::Lenient)?)
    }

    /// Разбирает текст config.yaml в строгом режиме (`thepost config check`).
    pub fn parse_strict(
        &self,
        src: &str,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> tp_contracts::Result<Config> {
        Ok(self.parse_with(src, env, CheckMode::Strict)?)
    }

    pub(crate) fn parse_with(
        &self,
        src: &str,
        env: impl IntoIterator<Item = (String, String)>,
        mode: CheckMode,
    ) -> Result<Config, ConfigError> {
        let mut tree: Value = serde_yaml_ng::from_str(src)?;
        // Пустой файл — пустое отображение; секции разбираются из его текста.
        let empty = tree.is_null();
        if empty {
            tree = Value::Mapping(Mapping::new());
        }
        let overridden = apply_env(&mut tree, env)?;
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        // Неизвестные параметры отбрасываются до разбора: секции с `deny_unknown_fields` разбираются дальше,
        // и ошибки типов в них не прячутся за первым неизвестным полем.
        let unknown = self.unknown_params(&tree);
        for path in &unknown {
            remove_path(&mut tree, path);
            let msg = ConfigError::Unknown(path_string(path)).to_string();
            match mode {
                CheckMode::Strict => errors.push(msg),
                CheckMode::Lenient => warnings.push(msg),
            }
        }
        // После переопределения или отбрасывания позиции в ошибках относятся к объединённому тексту;
        // путь параметра — точный.
        let merged;
        let src = if empty || overridden || !unknown.is_empty() {
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
                    errors.push(ConfigError::Missing((*path).to_owned()).to_string());
                }
                Some(_) => {
                    // Не строка: точное сообщение с позицией даст разбор строки.
                    note(&mut errors, at::<String>(src, &key));
                }
            }
        }

        // Тип `domain` уже проверен вместе с обязательными параметрами.
        let domain = lookup(&tree, &["domain"])
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let profile: Profile = note(&mut errors, at(src, &["profile"]))
            .flatten()
            .unwrap_or_default();
        let role_entries: Vec<RoleEntry> = note(&mut errors, at(src, &["roles"]))
            .flatten()
            .unwrap_or_else(|| vec![RoleEntry::Preset(RolePreset::All)]);
        let roles = RoleSet::expand(&role_entries);

        let mut sections = BTreeMap::new();
        for (name, def) in &self.sections {
            let present = lookup(&tree, &[name]).is_some_and(|v| !v.is_null());
            if let Some(section) = note(&mut errors, (def.parse)(present.then_some(src), &[name])) {
                sections.insert(*name, section);
            }
        }

        let mut modules = BTreeMap::new();
        let listed: Vec<String> = match lookup(&tree, &["modules"]) {
            Some(Value::Mapping(m)) => m.keys().map(yaml_key).collect(),
            _ => Vec::new(),
        };
        for name in &listed {
            if !self.modules.contains_key(name.as_str()) {
                let switch: Option<ModuleSwitch> =
                    note(&mut errors, at(src, &["modules", name])).flatten();
                if switch.is_some_and(|s| s.enabled) {
                    errors.push(ConfigError::NoModule(name.clone()).to_string());
                }
            }
        }
        for (name, def) in &self.modules {
            let path = ["modules", name];
            let present = lookup(&tree, &path).is_some_and(|v| !v.is_null());
            let switch = if present {
                match at::<ModuleSwitch>(src, &path) {
                    Ok(switch) => switch,
                    Err(e) => {
                        // Та же ошибка повторилась бы при разборе полей модуля.
                        errors.push(e.to_string());
                        continue;
                    }
                }
            } else {
                None
            };
            let enabled = switch.is_some_and(|s| s.enabled);
            if let Some(config) = note(&mut errors, (def.parse)(present.then_some(src), &path)) {
                modules.insert(*name, ModuleState { enabled, config });
            }
        }

        self.check_channels(&tree, &sections, profile, &mut errors);
        self.check_plaintext_secrets(&tree, profile, &mut errors);
        let secret_refs = self.secret_refs(&tree, &mut errors);

        if !errors.is_empty() {
            return Err(ConfigError::Invalid(errors));
        }
        for w in &warnings {
            tracing::warn!(warning = %w, "configuration parameter ignored");
        }
        Ok(Config {
            domain,
            profile,
            role_entries,
            roles,
            sections,
            modules,
            warnings,
            secret_refs,
            secrets: BTreeMap::new(),
        })
    }

    /// Секреты открытым текстом (обе проверки): параметр с именем из [`SECRET_KEYS`] со строкой или числом и —
    /// вне профиля dev — адрес с паролем (`scheme://user:password@…`, `password=`). Адреса секций с собственной
    /// проверкой ([`Self::add_check`]) не просматриваются: их проверяют крейты (`PoolConfig::check` и т. п.).
    fn check_plaintext_secrets(&self, tree: &Value, profile: Profile, errors: &mut Vec<String>) {
        let Value::Mapping(root) = tree else {
            return;
        };
        for (key, value) in root {
            let name = key.as_str().unwrap_or_default();
            if ["domain", "profile", "roles"].contains(&name) {
                continue;
            }
            let urls = profile != Profile::Dev && !self.checks.contains_key(name);
            let mut path = vec![Seg::Key(key.clone())];
            find_plaintext_secrets(value, &mut path, urls, errors);
        }
    }

    /// Ссылки на секреты во всех секциях. Неверная ссылка в секции без регистрации — ошибка (секции из реестра
    /// сообщают о ней при разборе).
    fn secret_refs(&self, tree: &Value, errors: &mut Vec<String>) -> BTreeSet<SecretRef> {
        let mut out = BTreeSet::new();
        let Value::Mapping(root) = tree else {
            return out;
        };
        for (key, value) in root {
            let report = !key
                .as_str()
                .is_some_and(|n| self.sections.contains_key(n) || n == "modules");
            let mut path = vec![Seg::Key(key.clone())];
            collect_refs(value, &mut path, report, &mut out, errors);
        }
        out
    }

    /// Открытые внутренние каналы вне профиля dev (решение «Шифрование любого обмена»): проверки секций
    /// ([`Self::add_check`]; адреса каналов проверяют крейты — `PoolConfig::check`, `S3Config::check` и т. п.)
    /// и `tls.allow_plaintext: true` в секциях без проверки.
    fn check_channels(
        &self,
        tree: &Value,
        sections: &BTreeMap<&'static str, AnySection>,
        profile: Profile,
        errors: &mut Vec<String>,
    ) {
        for (name, checks) in &self.checks {
            let Some(section) = sections.get(name) else {
                continue;
            };
            for check in checks {
                if let Err(e) = check(section, profile) {
                    errors.push(format!("{name}: {}", message(&e)));
                }
            }
        }
        if profile != Profile::Dev
            && let Value::Mapping(root) = tree
        {
            for (key, value) in root {
                let name = key.as_str().unwrap_or_default();
                if ["domain", "profile", "roles"].contains(&name) || self.checks.contains_key(name)
                {
                    continue;
                }
                let mut path = vec![Seg::Key(key.clone())];
                find_plaintext(value, &mut path, errors);
            }
        }
    }

    /// Неизвестные параметры: ключи верхнего уровня вне платформы и реестра, поля зарегистрированных секций
    /// и модулей, которых нет в их JSON Schema. Незарегистрированные `database`/`blob` и модули не проверяются.
    fn unknown_params(&self, tree: &Value) -> Vec<Vec<Seg>> {
        let mut out = Vec::new();
        let Value::Mapping(root) = tree else {
            return out;
        };
        // Схема с `$defs` как дерево YAML: обход без зависимости от serde_json.
        let schema = serde_yaml_ng::to_value(self.schema()).unwrap_or(Value::Null);
        let defs = schema.get("$defs").unwrap_or(&Value::Null);
        let props = |path: &[&str]| schema_at(&schema, path);
        for (key, value) in root {
            let name = key.as_str().unwrap_or_default();
            let mut path = vec![Seg::Key(key.clone())];
            if self.sections.contains_key(name) {
                let Some(s) = props(&[name]) else {
                    continue;
                };
                walk(value, &[s], defs, &mut path, &mut out);
            } else if name == "modules" {
                let Value::Mapping(listed) = value else {
                    continue;
                };
                for (module, config) in listed {
                    let registered = module
                        .as_str()
                        .filter(|m| self.modules.contains_key(*m))
                        .and_then(|m| props(&["modules", m]));
                    if let Some(s) = registered {
                        path.push(Seg::Key(module.clone()));
                        walk(config, &[s], defs, &mut path, &mut out);
                        path.pop();
                    }
                }
            } else if !CORE_KEYS.contains(&name) && !URL_SECTIONS.contains(&name) {
                out.push(path);
            }
        }
        out
    }

    /// JSON Schema config.yaml (draft 2020-12): все зарегистрированные секции и модули.
    pub fn schema(&self) -> Schema {
        let mut generator = SchemaSettings::draft2020_12().into_generator();
        let mut props = BTreeMap::new();
        props.insert("domain".to_owned(), generator.subschema_for::<String>());
        props.insert("profile".to_owned(), generator.subschema_for::<Profile>());
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
    profile: Profile,
    role_entries: Vec<RoleEntry>,
    roles: RoleSet,
    sections: BTreeMap<&'static str, AnySection>,
    modules: BTreeMap<&'static str, ModuleState>,
    warnings: Vec<String>,
    secret_refs: BTreeSet<SecretRef>,
    /// Значения ссылок после [`Config::resolve_secrets`].
    secrets: BTreeMap<SecretRef, Secret>,
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
            .field("profile", &self.profile)
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

    /// Профиль запуска (`profile`); по умолчанию `prod`.
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// Предупреждения мягкого режима: отброшенные неизвестные параметры.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
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

    /// Ссылки на секреты во всех секциях (`{ secret: "<путь>#<поле>" }`), без повторов.
    pub fn secret_refs(&self) -> impl Iterator<Item = &SecretRef> {
        self.secret_refs.iter()
    }

    /// Значение секрета, прочитанное [`Config::resolve_secrets`]. Ссылки нет в конфигурации или секреты
    /// не прочитаны — `NotFound`.
    pub fn secret(&self, secret: &SecretRef) -> tp_contracts::Result<Secret> {
        self.secrets.get(secret).cloned().ok_or(Error::NotFound)
    }

    /// Читает все секреты конфигурации из `store`. Ошибка — прочитанные значения не меняются: все ссылки
    /// недоступны временно — `Unavailable`, иначе `Invalid` со списком ссылок и причин (без значений).
    pub async fn resolve_secrets(&mut self, store: &dyn SecretStore) -> tp_contracts::Result<()> {
        let mut resolved = BTreeMap::new();
        let mut failed = Vec::new();
        let mut retryable = true;
        for r in &self.secret_refs {
            match store.get(r).await {
                Ok(s) => {
                    resolved.insert(r.clone(), s);
                }
                Err(e) => {
                    retryable &= e.is_retryable();
                    failed.push(format!("secret {r}: {}", message(&e)));
                }
            }
        }
        if failed.is_empty() {
            self.secrets = resolved;
            return Ok(());
        }
        for f in &failed {
            tracing::warn!(error = %f, "configuration secret not resolved");
        }
        if retryable {
            Err(Error::UNAVAILABLE)
        } else {
            Err(Error::Invalid(failed.join("\n")))
        }
    }

    /// Те же значения и аренды секретов, что у `other`.
    pub(crate) fn same_secrets(&self, other: &Config) -> bool {
        self.secrets.len() == other.secrets.len()
            && self
                .secrets
                .iter()
                .zip(&other.secrets)
                .all(|((ra, a), (rb, b))| {
                    ra == rb && a.expose() == b.expose() && a.lease() == b.lease()
                })
    }

    /// Аренды прочитанных секретов, без повторов (поля одного динамического пути делят аренду).
    pub(crate) fn secret_leases(&self) -> Vec<SecretLease> {
        let mut seen = BTreeSet::new();
        self.secrets
            .values()
            .filter_map(Secret::lease)
            .filter(|l| !l.ttl.is_zero() && seen.insert(l.id.clone()))
            .cloned()
            .collect()
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

fn check_fn<T: Section>(
    check: impl Fn(&T, Profile) -> tp_contracts::Result<()> + Send + Sync + 'static,
) -> CheckFn {
    Arc::new(move |section: &AnySection, profile: Profile| {
        let section = section
            .as_ref()
            .downcast_ref::<T>()
            .ok_or_else(|| Error::internal("config section type mismatch"))?;
        check(section, profile)
    })
}

/// Текст ошибки без префикса `invalid: `.
fn message(e: &Error) -> String {
    match e {
        Error::Invalid(m) => m.clone(),
        other => other.to_string(),
    }
}

/// Добавляет ошибку в список; `None` — была ошибка.
fn note<T, E: fmt::Display>(errors: &mut Vec<String>, r: Result<T, E>) -> Option<T> {
    r.map_err(|e| errors.push(e.to_string())).ok()
}

/// Шаг пути параметра в дереве YAML.
#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Key(Value),
    Index(usize),
}

/// Путь параметра для сообщений: `smtp.limits.bogus`, `smtp.relays[1].port`.
fn path_string(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(&yaml_key(k));
            }
            Seg::Index(i) => {
                out.push('[');
                out.push_str(&i.to_string());
                out.push(']');
            }
        }
    }
    out
}

fn remove_path(tree: &mut Value, path: &[Seg]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut node = tree;
    for seg in parents {
        let next = match (seg, node) {
            (Seg::Key(k), Value::Mapping(m)) => m.get_mut(k),
            (Seg::Index(i), Value::Sequence(s)) => s.get_mut(*i),
            _ => None,
        };
        let Some(next) = next else {
            return;
        };
        node = next;
    }
    if let (Seg::Key(k), Value::Mapping(m)) = (last, node) {
        m.shift_remove(k);
    }
}

/// Подсхема поля по пути `properties.<a>.properties.<b>…`.
fn schema_at<'s>(schema: &'s Value, path: &[&str]) -> Option<&'s Value> {
    path.iter()
        .try_fold(schema, |node, key| node.get("properties")?.get(*key))
}

/// Допустимые поля объекта по объединению схем (`$ref`, `allOf`/`anyOf`/`oneOf`).
#[derive(Default)]
struct Shape<'s> {
    props: BTreeMap<String, Vec<&'s Value>>,
    additional: Vec<&'s Value>,
    items: Vec<&'s Value>,
    /// Объект с произвольными полями.
    open: bool,
}

/// Глубина разворачивания `$ref`: защита от циклических схем.
const MAX_SCHEMA_DEPTH: usize = 32;

/// Ключевые слова схемы, при которых она сама не описывает «объект с любыми полями».
const STRUCTURED: &[&str] = &[
    "$ref",
    "allOf",
    "anyOf",
    "oneOf",
    "properties",
    "additionalProperties",
    "const",
    "enum",
];

fn collect<'s>(schema: &'s Value, defs: &'s Value, shape: &mut Shape<'s>, depth: usize) {
    if depth > MAX_SCHEMA_DEPTH {
        shape.open = true;
        return;
    }
    let map = match schema {
        Value::Bool(open) => {
            shape.open |= *open;
            return;
        }
        Value::Mapping(m) => m,
        _ => return,
    };
    if let Some(target) = map.get("$ref").and_then(Value::as_str) {
        match target
            .strip_prefix("#/$defs/")
            .and_then(|name| defs.get(name))
        {
            Some(def) => collect(def, defs, shape, depth + 1),
            None => shape.open = true,
        }
    }
    for kw in ["allOf", "anyOf", "oneOf"] {
        if let Some(Value::Sequence(subs)) = map.get(kw) {
            for sub in subs {
                collect(sub, defs, shape, depth + 1);
            }
        }
    }
    if let Some(Value::Mapping(props)) = map.get("properties") {
        for (k, s) in props {
            shape.props.entry(yaml_key(k)).or_default().push(s);
        }
    }
    match map.get("additionalProperties") {
        Some(Value::Bool(open)) => shape.open |= *open,
        Some(s @ Value::Mapping(_)) => shape.additional.push(s),
        _ => {}
    }
    if let Some(items) = map.get("items") {
        shape.items.push(items);
    }
    let object_type = match map.get("type") {
        None => true,
        Some(Value::String(t)) => t == "object",
        Some(Value::Sequence(ts)) => ts.iter().any(|t| t.as_str() == Some("object")),
        Some(_) => false,
    };
    // Объект без описания полей (`{}`, `{"type": "object"}`) принимает любые поля.
    if object_type && !STRUCTURED.iter().any(|k| map.contains_key(*k)) {
        shape.open = true;
    }
}

/// Собирает пути полей `value`, которых нет в объединении схем `schemas`.
fn walk(
    value: &Value,
    schemas: &[&Value],
    defs: &Value,
    path: &mut Vec<Seg>,
    out: &mut Vec<Vec<Seg>>,
) {
    let mut shape = Shape::default();
    for s in schemas {
        collect(s, defs, &mut shape, 0);
    }
    match value {
        Value::Mapping(m) => {
            for (k, child) in m {
                path.push(Seg::Key(k.clone()));
                match k.as_str().and_then(|n| shape.props.get(n)) {
                    Some(subs) => walk(child, subs, defs, path, out),
                    None if !shape.additional.is_empty() => {
                        walk(child, &shape.additional, defs, path, out);
                    }
                    None if shape.open => {}
                    None => out.push(path.clone()),
                }
                path.pop();
            }
        }
        Value::Sequence(items) if !shape.items.is_empty() => {
            for (i, item) in items.iter().enumerate() {
                path.push(Seg::Index(i));
                walk(item, &shape.items, defs, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

/// Секреты открытым текстом в поддереве (см. `SectionRegistry::check_plaintext_secrets`); `urls` — искать
/// пароли в адресах. Ошибку с тем же путём, уже найденную разбором секции, не повторяет.
fn find_plaintext_secrets(
    value: &Value,
    path: &mut Vec<Seg>,
    urls: bool,
    errors: &mut Vec<String>,
) {
    match value {
        Value::Mapping(m) => {
            for (k, child) in m {
                path.push(Seg::Key(k.clone()));
                let secret_key = k
                    .as_str()
                    .is_some_and(|k| SECRET_KEYS.iter().any(|s| k.eq_ignore_ascii_case(s)));
                let scalar = match child {
                    Value::String(s) => !s.is_empty(),
                    Value::Number(_) => true,
                    _ => false,
                };
                if secret_key && scalar {
                    let p = path_string(path);
                    let prefix = format!("{p}:");
                    if !errors.iter().any(|e| e.starts_with(&prefix)) {
                        errors.push(format!("{p}: {PLAINTEXT_SECRET}"));
                    }
                } else {
                    find_plaintext_secrets(child, path, urls, errors);
                }
                path.pop();
            }
        }
        Value::Sequence(items) => {
            for (i, item) in items.iter().enumerate() {
                path.push(Seg::Index(i));
                find_plaintext_secrets(item, path, urls, errors);
                path.pop();
            }
        }
        Value::String(s) if urls && url_has_password(s) => errors.push(format!(
            "{}: password in the address is allowed only with profile: dev; \
             use a reference {{ secret: \"<path>#<field>\" }}",
            path_string(path)
        )),
        _ => {}
    }
}

/// Адрес содержит пароль: `scheme://user:password@host…` или параметр `password=`.
fn url_has_password(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if rest[..authority_end]
        .rsplit_once('@')
        .is_some_and(|(userinfo, _)| userinfo.contains(':'))
    {
        return true;
    }
    rest[authority_end..]
        .split_once('?')
        .is_some_and(|(_, query)| {
            query.split(['&', '#']).any(|kv| {
                kv.split_once('=')
                    .map_or(kv, |(k, _)| k)
                    .eq_ignore_ascii_case("password")
            })
        })
}

/// Ссылки `{ secret: "<путь>#<поле>" }` в поддереве; `report` — сообщать о неверных ссылках.
fn collect_refs(
    value: &Value,
    path: &mut Vec<Seg>,
    report: bool,
    out: &mut BTreeSet<SecretRef>,
    errors: &mut Vec<String>,
) {
    match value {
        Value::Mapping(m) => {
            if m.len() == 1
                && let Some(Value::String(r)) = value.get("secret")
            {
                match r.parse::<SecretRef>() {
                    Ok(r) => {
                        out.insert(r);
                    }
                    Err(e) if report => {
                        errors.push(format!("{}: {}", path_string(path), message(&e)));
                    }
                    Err(_) => {}
                }
                return;
            }
            for (k, child) in m {
                path.push(Seg::Key(k.clone()));
                collect_refs(child, path, report, out, errors);
                path.pop();
            }
        }
        Value::Sequence(items) => {
            for (i, item) in items.iter().enumerate() {
                path.push(Seg::Index(i));
                collect_refs(item, path, report, out, errors);
                path.pop();
            }
        }
        _ => {}
    }
}

/// `tls.allow_plaintext: true` в поддереве: открытый внутренний канал, допустимый только в профиле dev.
fn find_plaintext(value: &Value, path: &mut Vec<Seg>, errors: &mut Vec<String>) {
    match value {
        Value::Mapping(m) => {
            for (k, child) in m {
                path.push(Seg::Key(k.clone()));
                if k.as_str() == Some("tls")
                    && child.get("allow_plaintext").and_then(Value::as_bool) == Some(true)
                {
                    errors.push(format!(
                        "{}.allow_plaintext: allowed only with profile: dev",
                        path_string(path)
                    ));
                }
                find_plaintext(child, path, errors);
                path.pop();
            }
        }
        Value::Sequence(items) => {
            for (i, item) in items.iter().enumerate() {
                path.push(Seg::Index(i));
                find_plaintext(item, path, errors);
                path.pop();
            }
        }
        _ => {}
    }
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
    use tp_contracts::tls::{InternalChannel, InternalTls};

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
        relays: Vec<Limits>,
    }

    #[derive(Debug, Default, Deserialize, JsonSchema, PartialEq)]
    #[serde(default)]
    struct ArchiveConfig {
        days: u32,
    }

    /// Секция внутреннего канала с полем `tls` (как у `tp-cluster`, `tp-blob`).
    #[derive(Debug, Default, Deserialize, JsonSchema, PartialEq)]
    #[serde(default, deny_unknown_fields)]
    struct ChannelConfig {
        url: String,
        tls: InternalTls,
    }

    fn registry() -> SectionRegistry {
        let mut r = SectionRegistry::new();
        r.add_section::<SmtpConfig>("smtp").expect("smtp");
        r.add_section::<ChannelConfig>("bus").expect("bus");
        r.add_module::<ArchiveConfig>("archive").expect("archive");
        r
    }

    fn parse(src: &str) -> Result<Config, ConfigError> {
        registry().parse_with(src, Vec::new(), CheckMode::Lenient)
    }

    fn strict(src: &str) -> Result<Config, ConfigError> {
        registry().parse_with(src, Vec::new(), CheckMode::Strict)
    }

    fn err(src: &str) -> String {
        parse(src).expect_err("must fail").to_string()
    }

    fn lines(e: ConfigError) -> Vec<String> {
        e.to_string().lines().map(str::to_owned).collect()
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
        // Все отсутствующие разом.
        assert_eq!(
            err(""),
            "database.url: required parameter is missing\nblob.url: required parameter is missing\ndomain: required parameter is missing"
        );
    }

    #[test]
    fn error_names_parameter_path() {
        let e = err(&format!("{MINIMAL}smtp:\n  limits:\n    max_size: big\n"));
        assert!(e.starts_with("smtp.limits.max_size: invalid type"), "{e}");
        assert!(e.contains("line 6"), "{e}");
        let e = err(&format!("{MINIMAL}roles: [mailbox, imapp]\n"));
        // `RoleEntry` — untagged enum: путь до параметра, без индекса элемента.
        assert!(e.starts_with("roles: ") && e.contains("line 4"), "{e}");
        let e = strict(&format!("{MINIMAL}smtp:\n  limits:\n    bogus: 1\n"))
            .expect_err("strict")
            .to_string();
        assert_eq!(e, "smtp.limits.bogus: unknown parameter");
        assert_eq!(
            strict(&format!("{MINIMAL}smtpp: {{}}\n"))
                .expect_err("strict")
                .to_string(),
            "smtpp: unknown parameter"
        );
        let e = strict(&format!(
            "{MINIMAL}smtp:\n  relays: [{{ max_size: 1 }}, {{ burst: 2, extra: 3 }}]\n"
        ))
        .expect_err("strict")
        .to_string();
        assert_eq!(e, "smtp.relays[1].extra: unknown parameter");
        let e = err(&format!("{MINIMAL}profile: staging\n"));
        assert!(e.starts_with("profile: unknown variant"), "{e}");
    }

    /// Приёмка PLT-09: один файл с неизвестными полями (секция, модуль, верхний уровень), ошибкой типа
    /// и открытыми внутренними каналами вне dev.
    #[test]
    fn strict_and_lenient_on_same_file() {
        let body = "smtp:\n  banner: hi\n  limits: { bogus: 1, burst: many }\n\
                    smtpp: {}\n\
                    modules:\n  archive: { enabled: true, days: 3, dayz: 4 }\n\
                    service_http: { allow_plaintext: true }\n\
                    bus: { url: redis://cache, tls: { allow_plaintext: true } }\n";
        let prod = format!("{MINIMAL}{body}");

        let all = lines(strict(&prod).expect_err("strict"));
        assert_eq!(all.len(), 6, "{all:#?}");
        for unknown in ["smtp.limits.bogus", "smtpp", "modules.archive.dayz"] {
            assert!(
                all.contains(&format!("{unknown}: unknown parameter")),
                "{unknown}: {all:#?}"
            );
        }
        assert!(
            all.iter()
                .any(|l| l.starts_with("smtp.limits.burst: invalid type")),
            "{all:#?}"
        );
        assert!(
            all.iter().any(|l| l
                .starts_with("service_http: allow_plaintext is allowed only with profile: dev")),
            "{all:#?}"
        );
        assert!(
            all.contains(&"bus.tls.allow_plaintext: allowed only with profile: dev".to_owned()),
            "{all:#?}"
        );

        // Мягкий режим: неизвестные поля не ошибки, остальное — ошибки.
        let soft = lines(parse(&prod).expect_err("lenient"));
        assert_eq!(soft.len(), 3, "{soft:#?}");
        assert!(
            soft.iter().all(|l| !l.contains("unknown parameter")),
            "{soft:#?}"
        );

        // Профиль dev, без ошибки типа: мягкий режим запускается с предупреждениями, строгий — нет.
        let dev = format!(
            "{MINIMAL}profile: dev\n{}",
            body.replace("burst: many", "burst: 2")
        );
        let cfg = parse(&dev).expect("lenient dev");
        assert_eq!(cfg.profile(), Profile::Dev);
        assert_eq!(
            cfg.warnings(),
            [
                "smtp.limits.bogus: unknown parameter",
                "smtpp: unknown parameter",
                "modules.archive.dayz: unknown parameter",
            ]
        );
        let smtp = cfg.section::<SmtpConfig>("smtp").expect("smtp");
        assert_eq!((smtp.banner.as_str(), smtp.limits.burst), ("hi", 2));
        assert_eq!(cfg.module::<ArchiveConfig>("archive").expect("m").days, 3);
        assert!(cfg.module_enabled("archive"));
        assert!(
            cfg.section::<ServiceHttpConfig>("service_http")
                .expect("service_http")
                .allow_plaintext
        );
        assert_eq!(lines(strict(&dev).expect_err("strict dev")).len(), 3);
        assert!(parse(MINIMAL).expect("clean").warnings().is_empty());
    }

    #[test]
    fn section_checks() {
        let mut r = registry();
        r.add_check::<ChannelConfig>("bus", |c, profile| {
            c.tls
                .check_url(InternalChannel::Valkey, &c.url, profile)
                .map(|_| ())
        })
        .expect("check");
        assert_eq!(
            r.add_check::<ChannelConfig>("nope", |_, _| Ok(())),
            Err(Error::NotFound)
        );
        let e = r
            .parse_with(
                &format!("{MINIMAL}bus: {{ url: redis://cache }}\n"),
                Vec::new(),
                CheckMode::Lenient,
            )
            .expect_err("plaintext")
            .to_string();
        assert!(e.starts_with("bus: Valkey: address without TLS"), "{e}");
        // Секция с проверкой не просматривается общим поиском: одна ошибка, не две.
        let e = r
            .parse_with(
                &format!(
                    "{MINIMAL}bus: {{ url: rediss://cache, tls: {{ allow_plaintext: true }} }}\n"
                ),
                Vec::new(),
                CheckMode::Strict,
            )
            .expect_err("allow_plaintext");
        assert_eq!(lines(e).len(), 1);
        let ok = format!(
            "{MINIMAL}profile: dev\nbus: {{ url: redis://cache, tls: {{ allow_plaintext: true }} }}\n"
        );
        assert!(r.parse_with(&ok, Vec::new(), CheckMode::Strict).is_ok());
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
        let cfg = registry()
            .parse_with(MINIMAL, env, CheckMode::Lenient)
            .expect("env");
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
            .parse_with(MINIMAL, env, CheckMode::Lenient)
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
        registry()
            .parse_with("", env, CheckMode::Lenient)
            .expect("env only");
    }

    #[test]
    fn duplicate_registration() {
        let mut r = registry();
        assert_eq!(r.add_section::<SmtpConfig>("smtp"), Err(Error::Conflict));
        assert_eq!(r.add_section::<SmtpConfig>("roles"), Err(Error::Conflict));
        assert_eq!(r.add_section::<SmtpConfig>("profile"), Err(Error::Conflict));
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
