//! Хранилище секретов OpenBao/Vault (PLT-12; решение 2026-10-30 «Секреты — только через хранилище секретов»):
//! секция `secrets`, клиент [`VaultStore`] — встроенная реализация [`SecretStore`] по HTTP API Vault,
//! проверки `thepost doctor` ([`doctor`]).
//!
//! Ссылка `<mount>/<путь>#<поле>` читается из KV v2 (`GET /v1/<mount>/data/<путь>`, поле из `data.data`); точки
//! монтирования из `dynamic_mounts` (database secrets engine и т. п., а также KV v1) читаются как есть
//! (`GET /v1/<mount>/<путь>`, поле из `data`) — с арендой, если хранилище её выдало. Все поля одного динамического
//! пути берутся из одной аренды (логин и пароль — из одной выдачи), пока аренда действует.
//!
//! Вход узла — ровно один способ: Kubernetes (токен сервисного аккаунта), сертификат TLS узла (mTLS), AppRole
//! (`secret_id` из файла, например учётные данные systemd) или готовый токен из файла (Vault Agent, dev).
//! Токен узла продлевается, когда прошло две трети срока; отказ продления или ответ 403 — новый вход.
//! Канал — только TLS с CA из `secrets.tls.ca_file`; открытый HTTP — профиль dev и явный `tls.allow_plaintext`.
//! HTTP — `reqwest` (решение 2026-10-31 «готовое вместо своего»): без прокси и перенаправлений (токен узла не
//! уходит на другой адрес), таймаут — на весь запрос, размер ответа ограничен `max_response_bytes`.
//!
//! ```yaml
//! secrets:
//!   url: https://openbao.example.ru:8200
//!   tls: { ca_file: /etc/thepost/tls/ca.pem }
//!   auth:
//!     approle:
//!       role_id_file: /etc/thepost/openbao/role_id
//!       secret_id_file: /run/credentials/thepost.service/openbao-secret-id
//! database:
//!   password: { secret: "thepost/db#password" }
//! ```

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{Client, Method, Url};
use rustls::crypto::CryptoProvider;
use rustls::{ClientConfig, RootCertStore};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tp_contracts::config::duration;
use tp_contracts::secrets::{Secret, SecretLease, SecretRef, SecretStore};
use tp_contracts::tls::Profile;
use tp_contracts::{Error, Result};

/// Секция `secrets`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsConfig {
    /// Адрес OpenBao/Vault (`https://openbao.example.ru:8200`). Не задан — встроенный клиент не используется.
    pub url: Option<String>,
    /// Пространство имён OpenBao/Vault (заголовок `X-Vault-Namespace`).
    pub namespace: Option<String>,
    /// Канал к хранилищу.
    pub tls: SecretsTlsConfig,
    /// Вход узла в хранилище: ровно один способ.
    pub auth: SecretsAuthConfig,
    /// Точки монтирования, которые читаются без KV v2 (`GET /v1/<путь>`): движки с арендами и KV v1.
    pub dynamic_mounts: Vec<String>,
    /// Время на один запрос к хранилищу (`10s`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub request_timeout: Duration,
    /// Предел размера ответа хранилища, байт.
    pub max_response_bytes: usize,
    /// Пороги проверок `thepost doctor`.
    pub doctor: SecretsDoctorConfig,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            url: None,
            namespace: None,
            tls: SecretsTlsConfig::default(),
            auth: SecretsAuthConfig::default(),
            dynamic_mounts: vec!["database".to_owned()],
            request_timeout: Duration::from_secs(10),
            max_response_bytes: 1024 * 1024,
            doctor: SecretsDoctorConfig::default(),
        }
    }
}

/// Подсекция `secrets.tls`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsTlsConfig {
    /// PEM с доверенными CA для сертификата хранилища (по умолчанию — системный набор).
    pub ca_file: PathBuf,
    /// Открытый HTTP (`http://`) — только профиль dev (OpenBao в dev-режиме); в журнал — предупреждение.
    pub allow_plaintext: bool,
}

impl Default for SecretsTlsConfig {
    fn default() -> Self {
        Self {
            ca_file: PathBuf::from("/etc/ssl/certs/ca-certificates.crt"),
            allow_plaintext: false,
        }
    }
}

/// Подсекция `secrets.auth`: задаётся ровно один способ входа.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsAuthConfig {
    /// Kubernetes auth: токен сервисного аккаунта пода (Helm).
    pub kubernetes: Option<KubernetesAuth>,
    /// Вход по сертификату TLS узла (mTLS, cert auth).
    pub cert: Option<CertAuth>,
    /// AppRole: `secret_id` из файла (установка без Kubernetes).
    pub approle: Option<AppRoleAuth>,
    /// Готовый токен из файла: sink Vault Agent или dev.
    pub token: Option<TokenAuth>,
}

impl SecretsAuthConfig {
    fn count(&self) -> usize {
        usize::from(self.kubernetes.is_some())
            + usize::from(self.cert.is_some())
            + usize::from(self.approle.is_some())
            + usize::from(self.token.is_some())
    }
}

/// Вход Kubernetes auth.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct KubernetesAuth {
    /// Точка монтирования метода.
    pub mount: String,
    /// Роль в хранилище.
    pub role: String,
    /// Токен сервисного аккаунта; перечитывается при каждом входе.
    pub jwt_file: PathBuf,
}

impl Default for KubernetesAuth {
    fn default() -> Self {
        Self {
            mount: "kubernetes".to_owned(),
            role: "thepost".to_owned(),
            jwt_file: PathBuf::from("/var/run/secrets/kubernetes.io/serviceaccount/token"),
        }
    }
}

/// Вход по сертификату TLS узла.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CertAuth {
    /// Точка монтирования метода.
    pub mount: String,
    /// Роль (сертификат) в хранилище; не задана — хранилище подбирает по сертификату.
    pub name: Option<String>,
    /// PEM с цепочкой сертификата узла (сертификат узла первым).
    pub cert_file: PathBuf,
    /// PEM с закрытым ключом узла.
    pub key_file: PathBuf,
}

impl Default for CertAuth {
    fn default() -> Self {
        Self {
            mount: "cert".to_owned(),
            name: None,
            cert_file: PathBuf::from("/etc/thepost/tls/fullchain.pem"),
            key_file: PathBuf::from("/etc/thepost/tls/privkey.pem"),
        }
    }
}

/// Вход AppRole.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AppRoleAuth {
    /// Точка монтирования метода.
    pub mount: String,
    /// `role_id` (не секрет); или `role_id_file`.
    pub role_id: Option<String>,
    /// Файл с `role_id`.
    pub role_id_file: Option<PathBuf>,
    /// Файл с `secret_id` (учётные данные systemd, том с правами только для thepost); перечитывается при входе.
    pub secret_id_file: PathBuf,
}

impl Default for AppRoleAuth {
    fn default() -> Self {
        Self {
            mount: "approle".to_owned(),
            role_id: None,
            role_id_file: None,
            secret_id_file: PathBuf::from("/run/credentials/thepost.service/openbao-secret-id"),
        }
    }
}

/// Готовый токен.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TokenAuth {
    /// Файл с токеном; перечитывается при каждом входе (токен обновляет Vault Agent).
    pub token_file: PathBuf,
}

impl Default for TokenAuth {
    fn default() -> Self {
        Self {
            token_file: PathBuf::from("/run/thepost/openbao-token"),
        }
    }
}

/// Подсекция `secrets.doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsDoctorConfig {
    /// Непродлеваемый токен узла с меньшим остатком срока — предупреждение (`1h`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub min_token_ttl: Duration,
}

impl Default for SecretsDoctorConfig {
    fn default() -> Self {
        Self {
            min_token_ttl: Duration::from_secs(3600),
        }
    }
}

impl SecretsConfig {
    /// Проверка секции (строгий и мягкий режимы): адрес, канал, ровно один способ входа.
    pub fn check(&self, profile: Profile) -> Result<()> {
        if self.tls.allow_plaintext && profile != Profile::Dev {
            return Err(Error::invalid(
                "tls.allow_plaintext: allowed only with profile: dev",
            ));
        }
        let Some(url) = &self.url else {
            return Ok(());
        };
        if !is_tls(&parse_url(url)?) {
            if profile != Profile::Dev {
                return Err(Error::invalid(
                    "url: the channel to the secret store is TLS only; use https://",
                ));
            }
            if !self.tls.allow_plaintext {
                return Err(Error::invalid(
                    "url: http:// requires tls.allow_plaintext: true (profile dev only)",
                ));
            }
        }
        if self.auth.count() != 1 {
            return Err(Error::invalid(
                "auth: exactly one method is required: kubernetes, cert, approle or token",
            ));
        }
        if let Some(a) = &self.auth.approle
            && a.role_id.is_some() == a.role_id_file.is_some()
        {
            return Err(Error::invalid(
                "auth.approle: exactly one of role_id and role_id_file is required",
            ));
        }
        let mounts = [
            self.auth.kubernetes.as_ref().map(|a| a.mount.as_str()),
            self.auth.cert.as_ref().map(|a| a.mount.as_str()),
            self.auth.approle.as_ref().map(|a| a.mount.as_str()),
        ];
        let segment = |s: &str| {
            !s.is_empty()
                && s.split('/').all(|p| {
                    !p.is_empty()
                        && p != "."
                        && p != ".."
                        && p.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                })
        };
        if mounts.into_iter().flatten().any(|m| !segment(m)) {
            return Err(Error::invalid("auth: invalid mount"));
        }
        if self
            .dynamic_mounts
            .iter()
            .any(|m| m.contains('/') || !segment(m))
        {
            return Err(Error::invalid(
                "dynamic_mounts: expected mount names like database",
            ));
        }
        if self
            .namespace
            .as_deref()
            .is_some_and(|n| n.is_empty() || n.bytes().any(|b| b.is_ascii_control()))
        {
            return Err(Error::invalid("namespace: invalid value"));
        }
        if self.request_timeout.is_zero() || self.max_response_bytes == 0 {
            return Err(Error::invalid(
                "request_timeout and max_response_bytes must be positive",
            ));
        }
        Ok(())
    }
}

/// Ошибка клиента хранилища; на границе — `tp_contracts::Error`.
#[derive(Debug, thiserror::Error)]
enum VaultError {
    #[error("{0}")]
    Config(String),
    #[error("secret store: {}", chain(.0))]
    Http(#[from] reqwest::Error),
    #[error("secret store request: {0}")]
    Request(&'static str),
    #[error("secret store response larger than {0} bytes")]
    TooLarge(usize),
    #[error("secret store answered {status}: {}", .errors.join("; "))]
    Status {
        status: u16,
        errors: Vec<String>,
        retry_after: Option<Duration>,
    },
    #[error("secret store response: {0}")]
    Response(&'static str),
    #[error("not found")]
    NotFound,
}

impl VaultError {
    fn status(&self) -> Option<u16> {
        match self {
            VaultError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

impl From<VaultError> for Error {
    fn from(e: VaultError) -> Self {
        match &e {
            VaultError::Config(m) => Error::Invalid(m.clone()),
            VaultError::Http(h) if h.is_builder() => Error::Invalid(e.to_string()),
            VaultError::Request(_) => Error::Invalid(e.to_string()),
            VaultError::Http(_) | VaultError::TooLarge(_) => Error::UNAVAILABLE,
            VaultError::Status {
                status,
                retry_after,
                ..
            } => match status {
                404 => Error::NotFound,
                401 | 403 => Error::Forbidden,
                400 => Error::Invalid(e.to_string()),
                412 | 429 | 500..=599 => Error::Unavailable {
                    retry_after: *retry_after,
                },
                _ => Error::Internal(e.to_string()),
            },
            VaultError::Response(_) => Error::Internal(e.to_string()),
            VaultError::NotFound => Error::NotFound,
        }
    }
}

/// Способ входа после проверки секции.
#[derive(Debug, Clone)]
enum Auth {
    Kubernetes(KubernetesAuth),
    Cert(CertAuth),
    AppRole(AppRoleAuth),
    Token(TokenAuth),
}

impl Auth {
    fn from_config(cfg: &SecretsAuthConfig) -> Option<Self> {
        if let Some(a) = &cfg.kubernetes {
            return Some(Auth::Kubernetes(a.clone()));
        }
        if let Some(a) = &cfg.cert {
            return Some(Auth::Cert(a.clone()));
        }
        if let Some(a) = &cfg.approle {
            return Some(Auth::AppRole(a.clone()));
        }
        cfg.token.as_ref().map(|a| Auth::Token(a.clone()))
    }

    fn name(&self) -> &'static str {
        match self {
            Auth::Kubernetes(_) => "kubernetes",
            Auth::Cert(_) => "cert",
            Auth::AppRole(_) => "approle",
            Auth::Token(_) => "token",
        }
    }
}

/// Токен узла.
#[derive(Debug, Clone)]
struct NodeToken {
    value: Secret,
    /// Срок от `at`; ноль — без срока.
    ttl: Duration,
    /// Срок при выдаче (входе): продление, давшее меньше трети, — новый вход (упёрлись в предельный срок).
    issued_ttl: Duration,
    renewable: bool,
    at: Instant,
}

impl NodeToken {
    fn expired(&self, now: Instant) -> bool {
        !self.ttl.is_zero() && now >= self.at + self.ttl
    }

    fn renew_due(&self, now: Instant) -> bool {
        self.renewable && !self.ttl.is_zero() && now >= self.at + self.ttl * 2 / 3
    }
}

/// Текущая аренда динамического пути: все поля — из одной выдачи.
#[derive(Debug)]
struct Issued {
    fields: BTreeMap<String, Secret>,
    lease: SecretLease,
    expires: Instant,
}

struct Inner {
    http: Client,
    /// Адрес хранилища (`https://host:port/`).
    base: Url,
    max_body: usize,
    namespace: Option<String>,
    auth: Auth,
    dynamic_mounts: Vec<String>,
    token: Mutex<Option<NodeToken>>,
    issued: Mutex<HashMap<String, Issued>>,
}

/// Клиент OpenBao/Vault — встроенная реализация [`SecretStore`]. Клоны разделяют токен узла.
#[derive(Clone)]
pub struct VaultStore {
    inner: Arc<Inner>,
}

impl fmt::Debug for VaultStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaultStore")
            .field("endpoint", &self.inner.base.as_str())
            .field("auth", &self.inner.auth.name())
            .finish_non_exhaustive()
    }
}

impl VaultStore {
    /// Клиент по секции `secrets` (читает CA и сертификат узла; в хранилище не обращается — вход при первом
    /// запросе). Секция без `url` или с ошибкой — `Invalid`.
    pub async fn new(cfg: &SecretsConfig, profile: Profile) -> Result<Self> {
        cfg.check(profile)?;
        let url = cfg
            .url
            .as_deref()
            .ok_or_else(|| Error::invalid("secrets.url is not set"))?;
        let base = parse_url(url)?;
        let auth = Auth::from_config(&cfg.auth)
            .ok_or_else(|| Error::invalid("secrets.auth: no method"))?;
        let builder = Client::builder()
            .timeout(cfg.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent("thepost");
        let builder = if is_tls(&base) {
            let cfg = cfg.clone();
            let tls = tokio::task::spawn_blocking(move || client_tls(&cfg))
                .await
                .map_err(|_| Error::internal("secrets: tls loader task failed"))??;
            builder.use_preconfigured_tls(tls).https_only(true)
        } else {
            tracing::warn!(
                url = %base,
                "secret store channel is NOT encrypted (profile dev, secrets.tls.allow_plaintext)"
            );
            builder
        };
        let http = builder.build().map_err(VaultError::from)?;
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                base,
                max_body: cfg.max_response_bytes,
                namespace: cfg.namespace.clone(),
                auth,
                dynamic_mounts: cfg.dynamic_mounts.clone(),
                token: Mutex::new(None),
                issued: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Запрос без повторного входа; `token` — `X-Vault-Token`. Тело ответа затирается после разбора.
    async fn request(
        &self,
        method: &str,
        path: &str,
        token: Option<&Secret>,
        body: Option<&Value>,
    ) -> std::result::Result<Value, VaultError> {
        let method =
            Method::from_bytes(method.as_bytes()).map_err(|_| VaultError::Request("method"))?;
        let url = self
            .inner
            .base
            .join(path)
            .map_err(|_| VaultError::Request("path"))?;
        let mut req = self
            .inner
            .http
            .request(method, url)
            .header(ACCEPT, "application/json")
            .header("X-Vault-Request", "true");
        if let Some(t) = token {
            let mut value =
                HeaderValue::from_str(t.expose()).map_err(|_| VaultError::Request("token"))?;
            value.set_sensitive(true);
            req = req.header("X-Vault-Token", value);
        }
        if let Some(ns) = &self.inner.namespace {
            req = req.header("X-Vault-Namespace", ns.as_str());
        }
        if let Some(b) = body {
            let bytes =
                serde_json::to_vec(b).map_err(|_| VaultError::Response("request encoding"))?;
            req = req.header(CONTENT_TYPE, "application/json").body(bytes);
        }
        let mut resp = req.send().await?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let mut raw = read_body(&mut resp, self.inner.max_body).await?;
        let result = if (200..300).contains(&status) {
            if raw.iter().all(u8::is_ascii_whitespace) {
                Ok(Value::Null)
            } else {
                serde_json::from_slice(&raw).map_err(|_| VaultError::Response("invalid JSON"))
            }
        } else {
            Err(VaultError::Status {
                status,
                errors: errors_of(&raw),
                retry_after,
            })
        };
        raw.fill(0);
        result
    }

    /// Запрос с токеном узла. Ответ 403 — токен мог быть отозван: новый вход и один повтор.
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> std::result::Result<Value, VaultError> {
        let token = self.token().await?;
        match self.request(method, path, Some(&token), body).await {
            Err(e) if e.status() == Some(403) => {
                self.forget(&token).await;
                let token = self.token().await?;
                self.request(method, path, Some(&token), body).await
            }
            other => other,
        }
    }

    /// Действующий токен узла: продление после двух третей срока, вход — если токена нет или он истёк.
    async fn token(&self) -> std::result::Result<Secret, VaultError> {
        let mut slot = self.inner.token.lock().await;
        let now = Instant::now();
        if let Some(t) = slot.as_ref()
            && !t.expired(now)
        {
            if !t.renew_due(now) {
                return Ok(t.value.clone());
            }
            match self.renew_self(t).await {
                Ok(renewed) => {
                    let value = renewed.value.clone();
                    *slot = Some(renewed);
                    return Ok(value);
                }
                Err(e) => {
                    tracing::info!(error = %e, "secret store token not renewed, logging in again");
                }
            }
        }
        *slot = None;
        let t = self.login().await?;
        let value = t.value.clone();
        *slot = Some(t);
        Ok(value)
    }

    /// Сбрасывает токен узла, если он не сменился с момента запроса.
    async fn forget(&self, token: &Secret) {
        let mut slot = self.inner.token.lock().await;
        if slot
            .as_ref()
            .is_some_and(|t| t.value.expose() == token.expose())
        {
            *slot = None;
        }
    }

    async fn renew_self(&self, t: &NodeToken) -> std::result::Result<NodeToken, VaultError> {
        let v = self
            .request(
                "POST",
                "/v1/auth/token/renew-self",
                Some(&t.value),
                Some(&json!({})),
            )
            .await?;
        let mut renewed = auth_token(&v, Some(&t.value))?;
        if renewed.ttl < t.issued_ttl / 3 {
            return Err(VaultError::Response("token is near its maximum TTL"));
        }
        renewed.issued_ttl = t.issued_ttl;
        Ok(renewed)
    }

    /// Вход узла способом из секции.
    async fn login(&self) -> std::result::Result<NodeToken, VaultError> {
        let (mount, body) = match &self.inner.auth {
            Auth::Kubernetes(a) => {
                let jwt = read_secret_file(&a.jwt_file).await?;
                (&a.mount, json!({ "role": a.role, "jwt": jwt.expose() }))
            }
            Auth::Cert(a) => (
                &a.mount,
                match &a.name {
                    Some(name) => json!({ "name": name }),
                    None => json!({}),
                },
            ),
            Auth::AppRole(a) => {
                let role_id = match (&a.role_id, &a.role_id_file) {
                    (Some(id), _) => Secret::new(id.clone()),
                    (None, Some(file)) => read_secret_file(file).await?,
                    (None, None) => {
                        return Err(VaultError::Config("auth.approle: no role_id".into()));
                    }
                };
                let secret_id = read_secret_file(&a.secret_id_file).await?;
                (
                    &a.mount,
                    json!({ "role_id": role_id.expose(), "secret_id": secret_id.expose() }),
                )
            }
            Auth::Token(a) => {
                let token = read_secret_file(&a.token_file).await?;
                let v = self
                    .request("GET", "/v1/auth/token/lookup-self", Some(&token), None)
                    .await?;
                let data = v.get("data").ok_or(VaultError::Response("no data"))?;
                let ttl = secs(data.get("ttl"));
                return Ok(NodeToken {
                    value: token,
                    ttl,
                    issued_ttl: ttl,
                    renewable: data.get("renewable").and_then(Value::as_bool) == Some(true),
                    at: Instant::now(),
                });
            }
        };
        let path = format!("/v1/auth/{mount}/login");
        let v = self.request("POST", &path, None, Some(&body)).await;
        drop(body);
        let t = auth_token(&v?, None)?;
        tracing::info!(method = self.inner.auth.name(), ttl = %duration::format(t.ttl), "secret store login");
        Ok(t)
    }

    /// KV v2: поле `field` последней версии `mount/rest`.
    async fn read_kv(
        &self,
        mount: &str,
        rest: &str,
        field: &str,
    ) -> std::result::Result<Secret, VaultError> {
        let v = self
            .call("GET", &format!("/v1/{mount}/data/{rest}"), None)
            .await?;
        field_value(v.get("data").and_then(|d| d.get("data")), field).map(Secret::new)
    }

    /// Чтение как есть (движки с арендами, KV v1); поля одного пути — из одной действующей аренды.
    async fn read_dynamic(
        &self,
        path: &str,
        field: &str,
    ) -> std::result::Result<Secret, VaultError> {
        let mut issued = self.inner.issued.lock().await;
        let now = Instant::now();
        if let Some(cur) = issued.get(path)
            && cur.expires > now
        {
            return cur
                .fields
                .get(field)
                .map(|s| s.clone().with_lease(cur.lease.clone()))
                .ok_or(VaultError::NotFound);
        }
        issued.remove(path);
        let v = self.call("GET", &format!("/v1/{path}"), None).await?;
        let data = v.get("data").and_then(Value::as_object);
        let lease_id = v
            .get("lease_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let ttl = secs(v.get("lease_duration"));
        if lease_id.is_empty() || ttl.is_zero() {
            return field_value(v.get("data"), field).map(Secret::new);
        }
        let lease = SecretLease {
            id: lease_id.to_owned(),
            ttl,
            renewable: v.get("renewable").and_then(Value::as_bool) == Some(true),
        };
        let fields: BTreeMap<String, Secret> = data
            .into_iter()
            .flatten()
            .filter_map(|(k, val)| text(val).map(|s| (k.clone(), Secret::new(s))))
            .collect();
        let value = fields.get(field).cloned();
        issued.insert(
            path.to_owned(),
            Issued {
                fields,
                lease: lease.clone(),
                expires: now + ttl,
            },
        );
        value
            .map(|s| s.with_lease(lease))
            .ok_or(VaultError::NotFound)
    }

    /// Обновляет или снимает запомненную аренду после продления.
    async fn note_lease(&self, id: &str, renewed: Option<&SecretLease>) {
        let mut issued = self.inner.issued.lock().await;
        let now = Instant::now();
        issued.retain(|_, cur| {
            if cur.lease.id != id {
                return true;
            }
            match renewed {
                Some(l) => {
                    cur.lease.ttl = l.ttl;
                    cur.expires = now + l.ttl;
                    true
                }
                None => false,
            }
        });
    }

    fn fail(&self, what: &dyn fmt::Display, e: VaultError) -> Error {
        match e.status() {
            Some(400 | 403 | 404) => {}
            _ if matches!(e, VaultError::NotFound) => {}
            _ => tracing::warn!(secret = %what, error = %e, "secret store request failed"),
        }
        e.into()
    }
}

#[async_trait]
impl SecretStore for VaultStore {
    async fn get(&self, secret: &SecretRef) -> Result<Secret> {
        let path = secret.path();
        let (mount, rest) = match path.split_once('/') {
            Some((mount, rest)) => (mount, Some(rest)),
            None => (path, None),
        };
        let result = if self.inner.dynamic_mounts.iter().any(|m| m == mount) {
            self.read_dynamic(path, secret.field()).await
        } else {
            match rest {
                Some(rest) => self.read_kv(mount, rest, secret.field()).await,
                // KV v2 без пути внутри точки монтирования — такого секрета нет.
                None => Err(VaultError::NotFound),
            }
        };
        result.map_err(|e| self.fail(secret, e))
    }

    async fn renew(&self, lease: &SecretLease) -> Result<SecretLease> {
        if !lease.renewable {
            return Err(Error::NotFound);
        }
        let body = json!({ "lease_id": lease.id });
        match self.call("PUT", "/v1/sys/leases/renew", Some(&body)).await {
            Ok(v) => {
                let ttl = secs(v.get("lease_duration"));
                if ttl.is_zero() {
                    self.note_lease(&lease.id, None).await;
                    return Err(Error::NotFound);
                }
                let renewed = SecretLease {
                    id: v
                        .get("lease_id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&lease.id)
                        .to_owned(),
                    ttl,
                    renewable: v.get("renewable").and_then(Value::as_bool) == Some(true),
                };
                self.note_lease(&lease.id, Some(&renewed)).await;
                Ok(renewed)
            }
            // Неизвестная, истёкшая или отозванная аренда: хранилище отвечает 400 (lease not found).
            Err(e) if matches!(e.status(), Some(400 | 404)) => {
                self.note_lease(&lease.id, None).await;
                Err(Error::NotFound)
            }
            Err(e) => Err(self.fail(&format_args!("lease {}", lease.id), e)),
        }
    }
}

/// Адрес хранилища `https://host[:port]` (`http://` — открытый канал): без пути, параметров и учётных данных.
fn parse_url(url: &str) -> Result<Url> {
    let bad = |m: &str| Error::invalid(format!("url: {m}"));
    let u = Url::parse(url).map_err(|e| bad(&format!("{e}, expected https://host[:port]")))?;
    if !matches!(u.scheme(), "https" | "http") {
        return Err(bad("expected https://host[:port]"));
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err(bad("host is missing"));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(bad("credentials in the address are not allowed"));
    }
    if u.path() != "/" {
        return Err(bad("path is not allowed, expected https://host[:port]"));
    }
    if u.query().is_some() || u.fragment().is_some() {
        return Err(bad("expected https://host[:port]"));
    }
    Ok(u)
}

fn is_tls(url: &Url) -> bool {
    url.scheme() == "https"
}

/// Тело ответа не больше `max` байт.
async fn read_body(
    resp: &mut reqwest::Response,
    max: usize,
) -> std::result::Result<Vec<u8>, VaultError> {
    let len = resp.content_length().unwrap_or(0);
    if len > max as u64 {
        return Err(VaultError::TooLarge(max));
    }
    let mut body = Vec::with_capacity(len as usize);
    while let Some(chunk) = resp.chunk().await? {
        if body.len().saturating_add(chunk.len()) > max {
            body.fill(0);
            return Err(VaultError::TooLarge(max));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Текст ошибки с причинами (у `reqwest` суть — в `source`).
fn chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

/// Конфигурация rustls клиента: CA из `tls.ca_file`, сертификат узла для входа cert. Блокирующий I/O.
fn client_tls(cfg: &SecretsConfig) -> Result<ClientConfig> {
    let provider = CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let mut roots = RootCertStore::empty();
    let (added, _) = roots.add_parsable_certificates(
        crate::tls::read_chain(&cfg.tls.ca_file)
            .map_err(|e| Error::invalid(format!("secrets.tls.ca_file: {e}")))?,
    );
    if added == 0 {
        return Err(Error::invalid(format!(
            "secrets.tls.ca_file: {}: no usable CA certificates",
            cfg.tls.ca_file.display()
        )));
    }
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::invalid(format!("secrets.tls: {e}")))?
        .with_root_certificates(roots);
    let config = match &cfg.auth.cert {
        Some(cert) => {
            let chain = crate::tls::read_chain(&cert.cert_file)
                .map_err(|e| Error::invalid(format!("secrets.auth.cert: {e}")))?;
            let key = crate::tls::read_key(&cert.key_file)
                .map_err(|e| Error::invalid(format!("secrets.auth.cert: {e}")))?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|e| Error::invalid(format!("secrets.auth.cert: {e}")))?
        }
        None => builder.with_no_client_auth(),
    };
    Ok(config)
}

/// Файл с секретом (токен, `secret_id`): содержимое без пробельных символов по краям.
async fn read_secret_file(path: &Path) -> std::result::Result<Secret, VaultError> {
    let mut raw = tokio::fs::read(path)
        .await
        .map_err(|e| VaultError::Config(format!("{}: {e}", path.display())))?;
    let value = std::str::from_utf8(&raw)
        .map(|s| Secret::new(s.trim()))
        .map_err(|_| VaultError::Config(format!("{}: not UTF-8", path.display())));
    raw.fill(0);
    let value = value?;
    if value.expose().is_empty() {
        return Err(VaultError::Config(format!("{}: empty", path.display())));
    }
    Ok(value)
}

/// Токен из ответа входа или продления (`auth.client_token`, `auth.lease_duration`, `auth.renewable`).
fn auth_token(v: &Value, current: Option<&Secret>) -> std::result::Result<NodeToken, VaultError> {
    let auth = v
        .get("auth")
        .ok_or(VaultError::Response("no auth in response"))?;
    let value = match auth.get("client_token").and_then(Value::as_str) {
        Some(t) if !t.is_empty() => Secret::new(t),
        _ => current
            .cloned()
            .ok_or(VaultError::Response("no client_token in response"))?,
    };
    let ttl = secs(auth.get("lease_duration"));
    Ok(NodeToken {
        value,
        ttl,
        issued_ttl: ttl,
        renewable: auth.get("renewable").and_then(Value::as_bool) == Some(true),
        at: Instant::now(),
    })
}

fn secs(v: Option<&Value>) -> Duration {
    Duration::from_secs(v.and_then(Value::as_u64).unwrap_or(0))
}

/// Значение поля как строка: строка — как есть, число и логическое — текстом, объект — JSON.
fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn field_value(data: Option<&Value>, field: &str) -> std::result::Result<String, VaultError> {
    data.and_then(|d| d.get(field))
        .and_then(text)
        .ok_or(VaultError::NotFound)
}

/// Тексты `errors` из ответа об ошибке (без тела целиком: в нём могут быть данные).
fn errors_of(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("errors").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.chars().take(200).collect())
                    .collect()
            })
        })
        .unwrap_or_default()
}

// ---------- doctor ----------

/// Итог проверки; порядок — по тяжести.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DoctorStatus {
    /// Не применяется в этой конфигурации.
    Skipped,
    /// Требование выполнено.
    Ok,
    /// Работать можно, но стоит исправить.
    Warn,
    /// Требование не выполнено.
    Fail,
}

/// Одна проверка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheck {
    /// Идентификатор: `secrets_config`, `secrets_store`, `secrets_seal`, `secrets_login`, `secrets_token`.
    pub id: &'static str,
    /// Итог.
    pub status: DoctorStatus,
    /// Фактическое значение (без секретов).
    pub value: String,
    /// Пояснение для администратора.
    pub message: String,
}

/// Отчёт проверок хранилища секретов.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DoctorReport {
    /// Проверки в порядке выполнения.
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// Худший итог; пропущенные проверки не ухудшают его (пустой отчёт — `Ok`).
    pub fn status(&self) -> DoctorStatus {
        self.checks
            .iter()
            .map(|c| c.status)
            .max()
            .map_or(DoctorStatus::Ok, |s| s.max(DoctorStatus::Ok))
    }

    /// Проверка по идентификатору.
    pub fn get(&self, id: &str) -> Option<&DoctorCheck> {
        self.checks.iter().find(|c| c.id == id)
    }

    pub(crate) fn push(
        &mut self,
        id: &'static str,
        status: DoctorStatus,
        value: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.checks.push(DoctorCheck {
            id,
            status,
            value: value.into(),
            message: message.into(),
        });
    }
}

/// Проверки `thepost doctor` для хранилища секретов: секция, доступность и распечатанность, способ распечатывания,
/// вход узла, срок токена узла. Не завершается ошибкой: всё — в отчёте.
pub async fn doctor(cfg: &SecretsConfig, profile: Profile) -> DoctorReport {
    use DoctorStatus::{Fail, Ok as Pass, Skipped, Warn};

    let mut r = DoctorReport::default();
    let Some(url) = &cfg.url else {
        r.push(
            "secrets_config",
            Skipped,
            "",
            "secrets.url is not set: the built-in OpenBao/Vault client is not used",
        );
        return r;
    };
    let store = match VaultStore::new(cfg, profile).await {
        Ok(store) => store,
        Err(e) => {
            r.push("secrets_config", Fail, url.clone(), message(&e));
            return r;
        }
    };
    if is_tls(&store.inner.base) {
        r.push("secrets_config", Pass, url.clone(), "TLS channel");
    } else {
        r.push(
            "secrets_config",
            Warn,
            url.clone(),
            "open channel (profile dev): secrets travel unencrypted",
        );
    }

    let health = store
        .request(
            "GET",
            "/v1/sys/health?standbyok=true&perfstandbyok=true",
            None,
            None,
        )
        .await;
    match health {
        Ok(v) => {
            let version = v.get("version").and_then(Value::as_str).unwrap_or_default();
            r.push(
                "secrets_store",
                Pass,
                version,
                "store is reachable and unsealed",
            );
        }
        Err(e) => {
            let message = match e.status() {
                Some(501) => "store is not initialized".to_owned(),
                Some(503) => "store is sealed".to_owned(),
                _ => e.to_string(),
            };
            r.push("secrets_store", Fail, "", message);
            return r;
        }
    }

    match store
        .request("GET", "/v1/sys/seal-status", None, None)
        .await
    {
        Ok(v) => {
            let kind = v
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let (status, message) = match kind.as_str() {
                "shamir" => (
                    Warn,
                    "manual unseal: mail stops after every store restart until an administrator unseals it",
                ),
                "static" | "file" => (
                    Warn,
                    "auto-unseal key is stored in a file on the server: secrets are protected weaker \
                     than with TPM/PKCS#11 or a cloud KMS",
                ),
                _ => (Pass, "auto-unseal"),
            };
            r.push("secrets_seal", status, kind, message);
        }
        Err(e) => r.push("secrets_seal", Warn, "", e.to_string()),
    }

    let token = match store.login().await {
        Ok(t) => {
            r.push(
                "secrets_login",
                Pass,
                store.inner.auth.name(),
                "node login succeeded",
            );
            t
        }
        Err(e) => {
            r.push(
                "secrets_login",
                Fail,
                store.inner.auth.name(),
                e.to_string(),
            );
            return r;
        }
    };
    let ttl = duration::format(token.ttl);
    let value = format!(
        "ttl {ttl}, {}",
        if token.renewable {
            "renewable"
        } else {
            "not renewable"
        }
    );
    if token.ttl.is_zero() {
        r.push(
            "secrets_token",
            Warn,
            "no expiry",
            "node token never expires: use a short-lived renewable token (AppRole, Kubernetes, cert)",
        );
    } else if !token.renewable && token.ttl < cfg.doctor.min_token_ttl {
        r.push(
            "secrets_token",
            Warn,
            value,
            format!(
                "node token is not renewable and expires in less than {}",
                duration::format(cfg.doctor.min_token_ttl)
            ),
        );
    } else {
        r.push("secrets_token", Pass, value, "node token lease");
    }
    r
}

fn message(e: &Error) -> String {
    match e {
        Error::Invalid(m) => m.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests;
