//! Вход администратора без OIDC: `thepost admin init` (ADM-11, решение владельца #193; CONTRACTS.md#Auth —
//! локальный токен Admin API) и вход по паролю (ADM-41, решение 2026-11-11, п. 117).
//!
//! - [`serve_init`] — обработчик одного соединения локального сокета узла: запрос — одна строка JSON
//!   ([`InitRequest`]), ответ — одна строка JSON ([`InitReply`]). Принимается только от root или учётной записи
//!   службы (`uid` собеседника по `SO_PEERCRED`; в контейнере без capabilities root не откроет сокет `0600`
//!   службы, поэтому `docker compose exec thepost thepost admin init` идёт от учётной записи службы). Сокет
//!   (`admin.init.socket`) и права на него — у вызывающего, `thepost`; [`serve_local`] — цикл приёма на Unix;
//!   клиент — [`request`].
//! - [`Admin::init`] — организация по умолчанию, принятый домен адреса (создаётся, если нет), учётная запись
//!   администратора с ролью администратора установки (явная запись, `AccountStore::grant_installation_admin`),
//!   пароль (пароль приложения с подписью [`INIT_PASSWORD_LABEL`]: вход IMAP, SMTP, JMAP и вход администратора по
//!   паролю) и одноразовый токен Admin API со сроком `admin.init.token_ttl`. Администратор уже заведён (есть учётная
//!   запись с этим адресом или ранее выпущенный токен) — отказ, `force` — повтор (прежние пароли `admin init`
//!   отзываются). Каждый вызов — в журнал аудита.
//! - `POST /api/v1/session` с телом [`PasswordLogin`] — вход администратора по имени входа и паролю (решение 117):
//!   пароль проверяет [`crate::Deps::auth`] — тот же путь, что вход пользователя на узле (пароль каталога или
//!   приложения под защитой от перебора `AbuseGuard` по адресу клиента и учётной записи). Роли сессии — по
//!   назначениям `admin.roles` ([`Admin::caller`]) и записи о роли администратора установки
//!   (`AccountStore::is_installation_admin`; ставит `admin init`). Подпись пароля роли не даёт. Без ролей — 403,
//!   неверный пароль или блокировка — 401, выключен (`admin.password_login`, [`crate::AdminConfig::password_login_enabled`]) — 404.
//! - `POST /api/v1/session` без тела — обмен токена `admin init` (`Authorization: Bearer tpa_…`) на сессию
//!   (решение 2026-11-11, п. 2): токен гасится первой проверкой; роль — администратор инсталляции. Работает и при
//!   выключенном входе по паролю: первый вход и вход без ролей в `admin.roles`.
//! - Сессия — значение `tps_…` со сроком `admin.init.session_ttl`, в памяти узла: после истечения или перезапуска —
//!   снова вход по паролю (или OIDC). Каждый вход и каждая неудача — в журнал аудита.
//! - `GET /api/v1/session` — без входа: способы входа консоли — поставщик OIDC ([`OidcLogin`]: издатель — тот, чьи
//!   токены проверяет узел, `auth.oidc.issuer` ([`crate::Deps::oidc_issuer`]); клиент консоли — `admin.oidc`,
//!   [`OidcClient`]; не настроен — `null`) и вход по паролю (`password`). Секретов в ответе нет: клиент консоли —
//!   публичный (код авторизации с PKCE).

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tp_contracts::admin_audit::AdminAuditRecord;
use tp_contracts::admin_token::{
    LOCAL_ADMIN_TOKEN_PREFIX, LocalAdminToken, LocalAdminTokens, is_local_admin_token,
};
use tp_contracts::auth::{Credentials, Principal};
use tp_contracts::config::duration;
use tp_contracts::meta::{Account, AccountKind, AccountScope, AccountUpsert};
use tp_contracts::transport::{AcceptedDomain, DomainKind};
use tp_contracts::{AccountId, DomainId, Error, Result, TenantId};
use utoipa::{OpenApi, ToSchema};

use crate::server::{Admin, AdminRole, ApiError, Caller, Problem};

/// Маршрут сессии: способы входа (`GET`), вход по паролю и обмен локального токена (`POST`).
pub const SESSION_ROUTE: &str = "/api/v1/session";

/// Префикс значения сессии Admin API в `Authorization: Bearer`.
pub const SESSION_PREFIX: &str = "tps_";

/// Подпись паролей, выпущенных `admin init` (по ней повтор `--force` отзывает прежние; роли не даёт).
pub const INIT_PASSWORD_LABEL: &str = "thepost admin init";

/// Маршрут записей аудита локального сокета.
const INIT_ROUTE: &str = "local:admin init";

/// Секция `admin.init`: первый администратор без OIDC.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct InitConfig {
    /// Срок одноразового токена Admin API.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub token_ttl: Duration,
    /// Срок сессии Admin API, полученной за токен.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub session_ttl: Duration,
    /// Наибольшее число действующих сессий на узле; при превышении вытесняется истекающая раньше всех.
    pub max_sessions: usize,
    /// Наибольший размер запроса локального сокета, байт.
    pub max_request_bytes: usize,
    /// Путь локального сокета узла (права `0600` учётной записи службы); каталог — доступный службе на запись.
    pub socket: PathBuf,
}

impl Default for InitConfig {
    fn default() -> Self {
        Self {
            token_ttl: Duration::from_secs(600),
            session_ttl: Duration::from_secs(8 * 3600),
            max_sessions: 64,
            max_request_bytes: 4096,
            socket: PathBuf::from("/var/lib/thepost/admin.sock"),
        }
    }
}

impl InitConfig {
    /// Проверяет значения: сроки и пределы больше нуля.
    pub fn check(&self) -> Result<()> {
        if self.token_ttl.is_zero() || self.session_ttl.is_zero() {
            return Err(Error::invalid(
                "admin.init: token_ttl и session_ttl — больше нуля",
            ));
        }
        if self.max_sessions == 0 || self.max_request_bytes == 0 {
            return Err(Error::invalid(
                "admin.init: max_sessions и max_request_bytes — больше нуля",
            ));
        }
        Ok(())
    }
}

/// Запрос `admin init`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitRequest {
    /// Адрес (имя входа) администратора; домен адреса становится принятым доменом организации.
    pub email: String,
    /// Отображаемое имя; нет — адрес.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Повтор при уже заведённом администраторе.
    #[serde(default)]
    pub force: bool,
}

/// Результат `admin init`: пароль и токен показываются один раз. `Debug` их не показывает.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitGranted {
    /// Организация.
    pub tenant: TenantId,
    /// Учётная запись администратора.
    pub account: AccountId,
    /// Имя входа.
    pub login: String,
    /// Пароль учётной записи (пароль приложения: IMAP, SMTP, JMAP).
    pub password: String,
    /// Одноразовый токен Admin API (`tpa_…`): обменять на сессию — `POST /api/v1/session`.
    pub token: String,
    /// Токен действует до.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

impl fmt::Debug for InitGranted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InitGranted")
            .field("tenant", &self.tenant)
            .field("account", &self.account)
            .field("login", &self.login)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Ответ локального сокета.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InitReply {
    /// Выполнено.
    Granted(InitGranted),
    /// Отказ: код в смысле HTTP (403 — не root, 409 — администратор уже есть, 400 — запрос, 503 — узел не готов)
    /// и причина для оператора.
    Refused {
        /// Код.
        code: u16,
        /// Причина.
        reason: String,
    },
}

impl InitReply {
    fn refused(e: &Error) -> Self {
        let (code, reason) = match e {
            Error::Conflict => (
                409,
                "администратор уже заведён (учётная запись или выпущенный токен); повтор — с --force".to_owned(),
            ),
            Error::Invalid(m) => (400, m.clone()),
            Error::Forbidden => (403, "только root на узле".to_owned()),
            Error::NotFound => (404, "не найдено".to_owned()),
            Error::QuotaExceeded => (507, "превышена квота".to_owned()),
            Error::Unavailable { .. } => (
                503,
                "Admin API узла не готов: хранилище локальных токенов или настройки транспорта не подключены".to_owned(),
            ),
            Error::Internal(m) => {
                tracing::error!(error = %m, "admin init: внутренняя ошибка");
                (500, "внутренняя ошибка, подробности — в журнале узла".to_owned())
            }
        };
        Self::Refused { code, reason }
    }
}

/// Сессия Admin API.
struct Session {
    principal: Principal,
    roles: BTreeSet<AdminRole>,
    expires_at: OffsetDateTime,
}

/// Локальный вход: хранилище токенов и сессии узла (по хэшу значения).
pub(crate) struct Local {
    tokens: Arc<dyn LocalAdminTokens>,
    sessions: Mutex<HashMap<String, Session>>,
}

impl Local {
    pub(crate) fn new(tokens: Arc<dyn LocalAdminTokens>) -> Self {
        Self {
            tokens,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Случайное значение с префиксом: 32 байта hex.
fn random_value(prefix: &str) -> Result<String> {
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let mut buf = [0u8; 32];
    provider
        .secure_random
        .fill(&mut buf)
        .map_err(|_| Error::internal("нет случайных чисел"))?;
    let mut out = String::with_capacity(prefix.len() + 64);
    out.push_str(prefix);
    for b in buf {
        out.push_str(&format!("{b:02x}"));
    }
    Ok(out)
}

/// Хэш значения: hex SHA-256 в нижнем регистре.
fn hash(value: &str) -> String {
    tp_mailstore::split::hash(value.as_bytes()).to_string()
}

/// Учётная запись без идентификатора (отказ до её выбора) — нулевой UUID.
fn nobody() -> AccountId {
    "00000000-0000-0000-0000-000000000000"
        .parse()
        .unwrap_or_default()
}

impl Admin {
    /// Подключает локальный вход: хранилище одноразовых токенов (`admin init`, `POST /api/v1/session`).
    pub fn with_local_tokens(mut self, tokens: Arc<dyn LocalAdminTokens>) -> Self {
        self.local = Some(Arc::new(Local::new(tokens)));
        self
    }

    /// Заводит первого администратора (см. описание модуля). Нет хранилища токенов или настроек транспорта —
    /// `Unavailable`; администратор уже заведён без `force` — `Conflict`; недопустимый адрес — `Invalid`.
    pub async fn init(&self, req: &InitRequest) -> Result<InitGranted> {
        let r = self.init_inner(req).await;
        let account = r.as_ref().map_or_else(|_| nobody(), |g| g.account);
        let status = match &r {
            Ok(_) => 201,
            Err(e) => match InitReply::refused(e) {
                InitReply::Refused { code, .. } => code,
                InitReply::Granted(_) => 500,
            },
        };
        let path = if req.force {
            "admin init --force"
        } else {
            "admin init"
        };
        self.audit_local(account, req.email.trim(), path, status)
            .await;
        r
    }

    async fn init_inner(&self, req: &InitRequest) -> Result<InitGranted> {
        let local = self.local.as_ref().ok_or(Error::UNAVAILABLE)?;
        let transport = self.transport.as_deref().ok_or(Error::UNAVAILABLE)?;
        let tenant = TenantId::DEFAULT;
        let (login, domain_name) = crate::accounts::split_login(&req.email)?;
        let existing = self.d.accounts.by_login(tenant, &login).await?;
        let issued = !local.tokens.list(tenant).await?.is_empty();
        if (existing.is_some() || issued) && !req.force {
            return Err(Error::Conflict);
        }

        let snap = transport.cluster().await?;
        let domain = match snap.domain(&domain_name) {
            Some(d) if d.tenant == tenant => d.id,
            Some(_) => {
                return Err(Error::invalid(format!(
                    "домен {domain_name} принадлежит другой организации"
                )));
            }
            None => {
                let d = AcceptedDomain {
                    id: DomainId::new(),
                    tenant,
                    name: domain_name,
                    kind: DomainKind::Authoritative,
                };
                transport.put_domain(snap.version, &d).await?;
                d.id
            }
        };

        let account = match existing {
            Some(a) if a.enabled => a,
            Some(a) => self.upsert(tenant, enabled(a)).await?,
            None => {
                let display_name = req
                    .display_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .unwrap_or(&login)
                    .to_owned();
                self.upsert(
                    tenant,
                    AccountUpsert {
                        domain,
                        login: login.clone(),
                        display_name,
                        external_id: None,
                        enabled: true,
                        aliases: Vec::new(),
                        kind: AccountKind::User,
                        hidden_from_gal: false,
                    },
                )
                .await?
            }
        };
        self.d
            .accounts
            .grant_installation_admin(tenant, account.id)
            .await?;
        let scope = AccountScope {
            tenant,
            account: account.id,
        };

        // Повтор: прежние пароли `admin init` больше не действуют.
        for p in self.d.app_passwords.list(scope).await? {
            if p.label == INIT_PASSWORD_LABEL && p.revoked_at.is_none() {
                self.d.app_passwords.revoke(scope, &p.id).await?;
            }
        }
        let (_, password) = self
            .d
            .app_passwords
            .issue(scope, INIT_PASSWORD_LABEL)
            .await?;

        let token = random_value(LOCAL_ADMIN_TOKEN_PREFIX)?;
        let now = OffsetDateTime::now_utc();
        let expires_at = now + self.cfg.init.token_ttl;
        let id = random_value("init-")?;
        local
            .tokens
            .issue(LocalAdminToken {
                id: id[..21].to_owned(),
                tenant,
                account: account.id,
                hash: hash(&token),
                created_at: now,
                expires_at,
                used_at: None,
                revoked_at: None,
            })
            .await?;
        tracing::info!(login = %account.login, "admin init: администратор заведён, выпущен токен Admin API");
        Ok(InitGranted {
            tenant,
            account: account.id,
            login: account.login,
            password: password.expose().to_owned(),
            token,
            expires_at,
        })
    }

    async fn upsert(&self, tenant: TenantId, data: AccountUpsert) -> Result<Account> {
        self.d.accounts.upsert(tenant, data).await
    }

    async fn audit_local(&self, actor: AccountId, login: &str, path: &str, status: u16) {
        let tenant = TenantId::DEFAULT;
        let record = AdminAuditRecord {
            at: OffsetDateTime::now_utc(),
            tenant: Some(tenant),
            actor_tenant: tenant,
            actor,
            actor_login: login.to_owned(),
            roles: Vec::new(),
            method: "LOCAL".into(),
            route: INIT_ROUTE.into(),
            path: path.into(),
            status,
            content_messages: None,
        };
        if let Err(e) = crate::audit::record(&self.d, &record).await {
            tracing::error!(error = %e, route = INIT_ROUTE, "admin: запись аудита не сохранена");
        }
    }

    /// Администратор по значению сессии в момент `now`; не сессия — `None`, нет или истекла — `Forbidden`.
    pub(crate) fn session_caller(
        &self,
        bearer: &str,
        now: OffsetDateTime,
    ) -> Option<Result<Caller>> {
        if !bearer.starts_with(SESSION_PREFIX) {
            return None;
        }
        let Some(local) = &self.local else {
            return Some(Err(Error::Forbidden));
        };
        let mut sessions = local.sessions();
        let key = hash(bearer);
        Some(match sessions.get(&key) {
            Some(s) if now < s.expires_at => Ok(Caller {
                principal: s.principal.clone(),
                roles: s.roles.clone(),
            }),
            Some(_) => {
                sessions.remove(&key);
                Err(Error::Forbidden)
            }
            None => Err(Error::Forbidden),
        })
    }

    /// Новая сессия субъекта с ролями: значение возвращается один раз.
    fn open_session(
        &self,
        local: &Local,
        principal: Principal,
        roles: BTreeSet<AdminRole>,
        now: OffsetDateTime,
    ) -> Result<(String, OffsetDateTime)> {
        let value = random_value(SESSION_PREFIX)?;
        let expires_at = now + self.cfg.init.session_ttl;
        let mut sessions = local.sessions();
        sessions.retain(|_, s| now < s.expires_at);
        while sessions.len() >= self.cfg.init.max_sessions {
            let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, s)| s.expires_at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            sessions.remove(&oldest);
        }
        sessions.insert(
            hash(&value),
            Session {
                principal,
                roles,
                expires_at,
            },
        );
        Ok((value, expires_at))
    }
}

fn enabled(a: Account) -> AccountUpsert {
    AccountUpsert {
        domain: a.domain,
        login: a.login,
        display_name: a.display_name,
        external_id: a.external_id,
        enabled: true,
        aliases: a.aliases,
        kind: a.kind,
        hidden_from_gal: a.hidden_from_gal,
    }
}

/// Обслуживает одно соединение локального сокета: `peer_uid` — `uid` собеседника (`SO_PEERCRED`); принимается
/// root (0) и `service_uid` (учётная запись службы), иначе — отказ. Ошибка — только ввода-вывода.
pub async fn serve_init<S>(
    admin: &Admin,
    stream: S,
    peer_uid: Option<u32>,
    service_uid: Option<u32>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let max = admin.cfg.init.max_request_bytes;
    let (r, mut w) = tokio::io::split(stream);
    let mut line = Vec::new();
    BufReader::new(r.take(max as u64 + 1))
        .read_until(b'\n', &mut line)
        .await?;
    let allowed = peer_uid.is_some_and(|u| u == 0 || Some(u) == service_uid);
    let reply = if !allowed {
        admin
            .audit_local(nobody(), &format!("uid {peer_uid:?}"), "admin init", 403)
            .await;
        InitReply::refused(&Error::Forbidden)
    } else if line.len() > max {
        InitReply::refused(&Error::invalid(
            "запрос длиннее admin.init.max_request_bytes",
        ))
    } else {
        match serde_json::from_slice::<InitRequest>(&line) {
            Ok(req) => match admin.init(&req).await {
                Ok(g) => InitReply::Granted(g),
                Err(e) => InitReply::refused(&e),
            },
            Err(e) => InitReply::refused(&Error::invalid(format!("запрос — не InitRequest: {e}"))),
        }
    };
    let mut out = serde_json::to_vec(&reply).map_err(std::io::Error::other)?;
    out.push(b'\n');
    w.write_all(&out).await?;
    w.shutdown().await
}

/// Цикл приёма локального сокета: каждое соединение — [`serve_init`] с `uid` собеседника по `SO_PEERCRED`;
/// `service_uid` — учётная запись службы (владелец сокета). Завершается при ошибке приёма.
#[cfg(unix)]
pub async fn serve_local(
    admin: Arc<Admin>,
    listener: tokio::net::UnixListener,
    service_uid: Option<u32>,
) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let admin = admin.clone();
        tokio::spawn(async move {
            let uid = stream.peer_cred().ok().map(|c| c.uid());
            if let Err(e) = serve_init(&admin, stream, uid, service_uid).await {
                tracing::warn!(error = %e, "admin init: соединение локального сокета");
            }
        });
    }
}

/// Клиент локального сокета: отправляет запрос и читает ответ (не больше `max` байт).
pub async fn request<S>(stream: S, req: &InitRequest, max: usize) -> Result<InitReply>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let io = |e: std::io::Error| Error::internal(format!("локальный сокет: {e}"));
    let (r, mut w) = tokio::io::split(stream);
    let mut out = serde_json::to_vec(req).map_err(|e| Error::internal(e.to_string()))?;
    out.push(b'\n');
    w.write_all(&out).await.map_err(io)?;
    let mut line = Vec::new();
    BufReader::new(r.take(max as u64))
        .read_until(b'\n', &mut line)
        .await
        .map_err(io)?;
    serde_json::from_slice(&line).map_err(|e| Error::internal(format!("ответ узла: {e}")))
}

/// Сессия Admin API: значение показывается один раз.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AdminSession {
    /// Значение для `Authorization: Bearer` (`tps_…`).
    pub session: String,
    /// Действует до.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub expires_at: OffsetDateTime,
    /// Организация.
    pub tenant: String,
    /// Учётная запись.
    pub account: String,
    /// Имя входа.
    pub login: String,
    /// Роли сессии.
    pub roles: Vec<AdminRole>,
}

/// Секция `admin.oidc`: клиент веб-консоли у поставщика OIDC (код авторизации с PKCE, публичный клиент). Поставщик —
/// `auth.oidc`: его издателя узел передаёт в [`crate::Deps::oidc_issuer`]; без `auth.oidc` секция — ошибка запуска.
///
/// ```yaml
/// oidc: { client_id: thepost-console, scope: "openid profile" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OidcClient {
    /// Идентификатор публичного клиента консоли у поставщика.
    pub client_id: String,
    /// Запрашиваемые области через пробел; нет — `openid`.
    #[serde(default)]
    pub scope: Option<String>,
}

impl OidcClient {
    /// Проверяет значения: клиент и области — непустая строка без управляющих знаков.
    pub fn check(&self) -> Result<()> {
        let plain = |s: &str| !s.trim().is_empty() && !s.chars().any(char::is_control);
        if !plain(&self.client_id) || !self.scope.as_deref().is_none_or(plain) {
            return Err(Error::invalid(
                "admin.oidc: client_id и scope — непустые строки без управляющих знаков",
            ));
        }
        Ok(())
    }
}

/// Вход в консоль через поставщика OIDC: издатель из `auth.oidc`, клиент из `admin.oidc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct OidcLogin {
    /// Издатель (`iss`) — тот, чьи токены проверяет Admin API (`auth.oidc.issuer`).
    pub issuer: String,
    /// Идентификатор публичного клиента консоли у поставщика.
    pub client_id: String,
    /// Запрашиваемые области через пробел; нет — `openid`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Способы входа консоли.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SessionInfo {
    /// Поставщик OIDC; `null` — не настроен.
    pub oidc: Option<OidcLogin>,
    /// Вход по паролю администратора включён (`POST /api/v1/session` с телом `PasswordLogin`, `admin.password_login`).
    pub password: bool,
}

/// Наибольшая длина имени входа, символов: длина адреса (RFC 3696, errata 1690).
const MAX_LOGIN_CHARS: usize = 320;

/// Вход администратора по паролю.
// `Debug` — без пароля.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PasswordLogin {
    /// Имя входа (адрес учётной записи).
    pub login: String,
    /// Пароль: каталога (`auth.ldap`) или приложения, в том числе выданный `thepost admin init`.
    #[schema(format = Password)]
    pub password: String,
}

impl fmt::Debug for PasswordLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordLogin")
            .field("login", &self.login)
            .finish_non_exhaustive()
    }
}

/// Маршрут сессии (вне слоя доступа: способы входа — без входа, вход — паролем или самим локальным токеном).
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route(SESSION_ROUTE, get(session_info).post(create_session))
}

/// Спецификация OpenAPI маршрута сессии.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(session_info, create_session),
    components(schemas(AdminSession, SessionInfo, OidcLogin, PasswordLogin))
)]
pub(crate) struct Doc;

/// Способы входа консоли: поставщик OIDC (издатель `auth.oidc`, клиент `admin.oidc`) и вход по паролю
/// (`admin.password_login`). Без входа.
#[utoipa::path(get, path = "/api/v1/session", tag = "admin", security(()),
    responses((status = 200, body = SessionInfo)))]
async fn session_info(State(a): State<Arc<Admin>>) -> Json<SessionInfo> {
    let oidc = a
        .cfg
        .oidc
        .as_ref()
        .zip(a.d.oidc_issuer.as_ref())
        .map(|(c, issuer)| OidcLogin {
            issuer: issuer.clone(),
            client_id: c.client_id.clone(),
            scope: c.scope.clone(),
        });
    Json(SessionInfo {
        oidc,
        password: a.cfg.password_login_enabled(),
    })
}

/// Новая сессия Admin API. С телом `PasswordLogin` — вход администратора по паролю (решение 117): неверный пароль
/// или блокировка перебора — 401, нет ролей администрирования — 403, вход по паролю выключен
/// (`admin.password_login`) — 404. Без тела — обмен одноразового токена `admin init`
/// (`Authorization: Bearer tpa_…`): токен гасится первой проверкой; повтор, срок, отзыв — 401. Каждая попытка — в
/// журнал аудита.
#[utoipa::path(post, path = "/api/v1/session", tag = "admin", security((), ("bearer" = [])),
    request_body(content = Option<PasswordLogin>,
        description = "Вход по паролю; без тела — обмен токена `admin init` (`Authorization: Bearer tpa_…`)"),
    responses((status = 201, body = AdminSession), (status = 400, body = Problem),
        (status = 401, description = "неверный пароль или токен, блокировка перебора", body = Problem),
        (status = 403, description = "у учётной записи нет ролей администрирования", body = Problem),
        (status = 404, description = "вход по паролю выключен (`admin.password_login`)", body = Problem),
        (status = 503, body = Problem)))]
async fn create_session(State(a): State<Arc<Admin>>, req: Request) -> Response {
    // Адрес клиента соединения ставит сервер HTTP (`tp_platform::service_http`): по нему — защита от перебора.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let (parts, body) = req.into_parts();
    let now = OffsetDateTime::now_utc();
    let attempt = match axum::body::to_bytes(body, a.cfg.max_body_size).await {
        Ok(body) if body.is_empty() => exchange(&a, &parts.headers, now).await,
        Ok(body) => password_login(&a, &body, peer, now).await,
        Err(_) => Attempt::anonymous(
            String::new(),
            Err(Denied::Invalid(
                "тело запроса больше admin.max_body_size".into(),
            )),
        ),
    };
    let status = attempt.status();
    let record = AdminAuditRecord {
        at: now,
        tenant: Some(attempt.tenant),
        actor_tenant: attempt.tenant,
        actor: attempt.actor,
        actor_login: attempt.login,
        roles: attempt
            .result
            .as_ref()
            .map(|s| s.roles.iter().map(|r| r.as_str().to_owned()).collect())
            .unwrap_or_default(),
        method: "POST".into(),
        route: SESSION_ROUTE.into(),
        path: SESSION_ROUTE.into(),
        status: status.as_u16(),
        content_messages: None,
    };
    if let Err(e) = crate::audit::record(&a.d, &record).await {
        tracing::error!(error = %e, route = SESSION_ROUTE, "admin: запись аудита не сохранена");
    }
    match attempt.result {
        Ok(s) => {
            let mut resp = (StatusCode::CREATED, Json(s)).into_response();
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err(Denied::Credentials(e)) => crate::server::auth_error(&e),
        Err(Denied::Disabled) => ApiError(Error::NotFound).into_response(),
        Err(Denied::Invalid(m)) => ApiError(Error::Invalid(m)).into_response(),
        Err(Denied::NoRole) => ApiError(Error::Forbidden).into_response(),
    }
}

/// Отказ во входе.
#[derive(Debug)]
enum Denied {
    /// Вход по паролю выключен (`admin.password_login`).
    Disabled,
    /// Тело запроса.
    Invalid(String),
    /// Учётные данные не приняты (`Forbidden` — 401) или временная ошибка (её код).
    Credentials(Error),
    /// Учётные данные верны, ролей администрирования нет.
    NoRole,
}

/// Попытка входа для журнала аудита: субъект (известен после проверки учётных данных) и исход.
struct Attempt {
    tenant: TenantId,
    actor: AccountId,
    login: String,
    result: std::result::Result<AdminSession, Denied>,
}

impl Attempt {
    /// Попытка без проверенного субъекта: в журнал — имя входа попытки.
    fn anonymous(login: String, result: std::result::Result<AdminSession, Denied>) -> Self {
        Self {
            tenant: TenantId::DEFAULT,
            actor: nobody(),
            login,
            result,
        }
    }

    fn status(&self) -> StatusCode {
        match &self.result {
            Ok(_) => StatusCode::CREATED,
            Err(Denied::Disabled) => StatusCode::NOT_FOUND,
            Err(Denied::Invalid(_)) => StatusCode::BAD_REQUEST,
            Err(Denied::Credentials(e)) if e.is_retryable() => StatusCode::SERVICE_UNAVAILABLE,
            Err(Denied::Credentials(_)) => StatusCode::UNAUTHORIZED,
            Err(Denied::NoRole) => StatusCode::FORBIDDEN,
        }
    }
}

/// Вход по паролю (см. [`create_session`]).
async fn password_login(
    a: &Admin,
    body: &[u8],
    peer: Option<IpAddr>,
    now: OffsetDateTime,
) -> Attempt {
    let req = match serde_json::from_slice::<PasswordLogin>(body) {
        Ok(r) => r,
        Err(e) => {
            return Attempt::anonymous(
                String::new(),
                Err(Denied::Invalid(format!(
                    "тело запроса — не PasswordLogin: {e}"
                ))),
            );
        }
    };
    let login: String = req.login.trim().chars().take(MAX_LOGIN_CHARS + 1).collect();
    if !a.cfg.password_login_enabled() {
        return Attempt::anonymous(login, Err(Denied::Disabled));
    }
    if login.is_empty() || login.chars().count() > MAX_LOGIN_CHARS || req.password.is_empty() {
        let m = format!("login — от 1 до {MAX_LOGIN_CHARS} символов, password — непустой");
        return Attempt::anonymous(login, Err(Denied::Invalid(m)));
    }
    let Some(local) = a.local.as_deref() else {
        return Attempt::anonymous(login, Err(Denied::Credentials(Error::UNAVAILABLE)));
    };
    let credentials = Credentials::Password {
        login: login.clone(),
        secret: req.password,
    };
    let principal = match a.d.auth.verify(credentials, peer).await {
        Ok(p) => p,
        Err(e) => return Attempt::anonymous(login, Err(Denied::Credentials(e))),
    };
    let subject = |result| Attempt {
        tenant: principal.tenant,
        actor: principal.account,
        login: principal.login.clone(),
        result,
    };
    let mut roles = a.caller(principal.clone()).roles;
    match a
        .d
        .accounts
        .is_installation_admin(principal.tenant, principal.account)
        .await
    {
        Ok(true) => {
            roles.insert(AdminRole::InstallationAdmin);
        }
        Ok(false) => {}
        Err(e) => return subject(Err(Denied::Credentials(e))),
    }
    if roles.is_empty() {
        return subject(Err(Denied::NoRole));
    }
    let result = a
        .open_session(local, principal.clone(), roles.clone(), now)
        .map(|(session, expires_at)| {
            tracing::info!(login = %principal.login, "admin: вход по паролю, открыта сессия");
            AdminSession {
                session,
                expires_at,
                tenant: principal.tenant.to_string(),
                account: principal.account.to_string(),
                login: principal.login.clone(),
                roles: roles.into_iter().collect(),
            }
        })
        .map_err(Denied::Credentials);
    subject(result)
}

/// Обмен токена `admin init` (см. [`create_session`]).
async fn exchange(a: &Admin, headers: &HeaderMap, now: OffsetDateTime) -> Attempt {
    let Some(local) = a.local.as_deref() else {
        return Attempt::anonymous(String::new(), Err(Denied::Credentials(Error::UNAVAILABLE)));
    };
    match exchange_inner(a, local, headers, now).await {
        Ok(s) => Attempt {
            tenant: TenantId::DEFAULT,
            actor: s.account.parse().unwrap_or_else(|_| nobody()),
            login: s.login.clone(),
            result: Ok(s),
        },
        Err(e) => Attempt::anonymous(String::new(), Err(Denied::Credentials(e))),
    }
}

async fn exchange_inner(
    a: &Admin,
    local: &Local,
    headers: &HeaderMap,
    now: OffsetDateTime,
) -> Result<AdminSession> {
    let bearer = crate::server::bearer(headers)?;
    if !is_local_admin_token(bearer) {
        return Err(Error::Forbidden);
    }
    let token = local.tokens.consume(&hash(bearer), now).await?;
    let account = match a.d.accounts.get(token.tenant, token.account).await? {
        Some(acc) if acc.enabled => acc,
        _ => return Err(Error::Forbidden),
    };
    let principal = Principal {
        tenant: account.tenant,
        account: account.id,
        login: account.login.clone(),
        groups: BTreeSet::new(),
        scopes: None,
    };
    let roles = BTreeSet::from([AdminRole::InstallationAdmin]);
    let (session, expires_at) = a.open_session(local, principal, roles.clone(), now)?;
    tracing::info!(login = %account.login, "admin: локальный токен обменян на сессию");
    Ok(AdminSession {
        session,
        expires_at,
        tenant: account.tenant.to_string(),
        account: account.id.to_string(),
        login: account.login,
        roles: roles.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tp_contracts::auth::Credentials;
    use tp_contracts::meta::AccountStore;
    use tp_contracts::transport::TransportStore;

    use super::*;
    use crate::testenv::Env;

    fn req(email: &str, force: bool) -> InitRequest {
        InitRequest {
            email: email.into(),
            display_name: None,
            force,
        }
    }

    /// Субъект пароля `secret` для учётной записи `scope` (FakeAuthenticator — вместо проверки узла).
    fn add_password(env: &Env, scope: AccountScope, login: &str, secret: &str, groups: &[&str]) {
        let p = Principal {
            account: scope.account,
            ..crate::testenv::subject(scope.tenant, login, groups)
        };
        env.auth.add_password(login, secret, p);
    }

    fn password_body(login: &str, password: &str) -> serde_json::Value {
        json!({"login": login, "password": password})
    }

    /// Приёмка ADM-41: верный пароль — сессия с ролями по `admin.roles`; неверный — 401 и запись в аудит с именем
    /// входа попытки.
    #[tokio::test]
    async fn password_login_opens_session() {
        let env = Env::new();
        let scope = env.account("admin@example.org").await;
        add_password(&env, scope, "admin@example.org", "s3cret", &["staff"]);

        let body = password_body(" Admin@Example.org ", "s3cret");
        let (s, h, b) = env.call("POST", SESSION_ROUTE, None, Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert_eq!(b["login"], "admin@example.org");
        assert_eq!(b["account"], scope.account.to_string());
        assert_eq!(b["roles"], json!(["organization_admin"]));
        let session = b["session"].as_str().expect("session").to_owned();
        assert!(session.starts_with(SESSION_PREFIX), "{session}");
        let (s, _, b) = env
            .call("GET", "/api/v1/whoami", Some(&session), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["roles"], json!(["organization_admin"]));

        let body = password_body("admin@example.org", "wrong");
        let (s, h, _) = env.call("POST", SESSION_ROUTE, None, Some(body)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(h[header::WWW_AUTHENTICATE], "Bearer realm=\"admin\"");

        let log = env.audit.records();
        let got: Vec<_> = log
            .iter()
            .map(|r| (r.route.as_str(), r.status, r.actor_login.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (SESSION_ROUTE, 201, "admin@example.org"),
                (SESSION_ROUTE, 401, "admin@example.org"),
            ]
        );
        assert_eq!(log[0].actor, scope.account);
        assert_eq!(log[0].roles, vec!["organization_admin".to_owned()]);
        assert_eq!(log[1].actor, nobody());
        assert!(log[1].roles.is_empty());
    }

    /// Пароль верен, ролей администрирования нет — 403 и запись в аудит; пароль не принят узлом — сессии нет.
    #[tokio::test]
    async fn password_login_without_role_is_forbidden() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        add_password(&env, scope, "user@example.org", "s3cret", &["staff"]);
        let body = password_body("user@example.org", "s3cret");
        let (s, _, b) = env.call("POST", SESSION_ROUTE, None, Some(body)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        assert!(b.get("session").is_none(), "{b}");
        let log = env.audit.records();
        assert_eq!((log[0].status, log[0].actor), (403, scope.account));
        assert_eq!(env.admin.local.as_ref().expect("local").sessions().len(), 0);
    }

    /// После `admin init` вход по паролю даёт роль администратора установки без `admin.roles`: роль — запись
    /// учётной записи, а не признак пароля, — отзыв паролей `admin init` её не снимает (вход решает проверка пароля).
    #[tokio::test]
    async fn init_admin_logs_in_by_password() {
        let env = Env::new();
        let g = env
            .admin
            .init(&req("boss@example.test", false))
            .await
            .expect("init");
        assert_eq!(
            env.meta
                .is_installation_admin(TenantId::DEFAULT, g.account)
                .await,
            Ok(true)
        );
        let scope = AccountScope {
            tenant: TenantId::DEFAULT,
            account: g.account,
        };
        add_password(&env, scope, &g.login, &g.password, &[]);
        let body = password_body(&g.login, &g.password);
        let (s, _, b) = env
            .call("POST", SESSION_ROUTE, None, Some(body.clone()))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        assert_eq!(b["roles"], json!(["installation_admin"]));

        for p in env.app_passwords.list(scope).await.expect("list") {
            env.app_passwords
                .revoke(scope, &p.id)
                .await
                .expect("revoke");
        }
        let (s, _, b) = env.call("POST", SESSION_ROUTE, None, Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        assert_eq!(b["roles"], json!(["installation_admin"]));
    }

    /// Пароль приложения с подписью [`INIT_PASSWORD_LABEL`], выпущенный не `admin init` (Admin API), роли не даёт:
    /// вход по нему без `admin.roles` — 403.
    #[tokio::test]
    async fn init_label_gives_no_role() {
        let env = Env::new();
        let scope = env.account("mallory@example.org").await;
        let path = format!(
            "/api/v1/orgs/{}/accounts/{}/app-passwords",
            scope.tenant, scope.account
        );
        let (s, _, b) = env
            .call(
                "POST",
                &path,
                Some("root"),
                Some(json!({"label": INIT_PASSWORD_LABEL})),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        let password = b["password"].as_str().expect("password").to_owned();
        let labels: Vec<_> = env
            .app_passwords
            .list(scope)
            .await
            .expect("list")
            .into_iter()
            .map(|p| p.label)
            .collect();
        assert_eq!(labels, [INIT_PASSWORD_LABEL]);
        add_password(&env, scope, "mallory@example.org", &password, &[]);
        let body = password_body("mallory@example.org", &password);
        let (s, _, b) = env.call("POST", SESSION_ROUTE, None, Some(body)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        assert_eq!(
            env.meta
                .is_installation_admin(scope.tenant, scope.account)
                .await,
            Ok(false)
        );
    }

    /// `admin.password_login`: по умолчанию включён без `admin.oidc`, выключен с ним; выключен — 404 без проверки
    /// пароля, запись в аудит; `GET /api/v1/session` сообщает консоли.
    #[tokio::test]
    async fn password_login_setting() {
        let oidc = || {
            Some(OidcClient {
                client_id: "thepost-console".into(),
                scope: None,
            })
        };
        for (password_login, oidc, enabled) in [
            (None, None, true),
            (None, oidc(), false),
            (Some(false), None, false),
            (Some(true), oidc(), true),
        ] {
            let cfg = crate::server::AdminConfig {
                password_login,
                oidc,
                ..crate::testenv::config()
            };
            assert_eq!(cfg.password_login_enabled(), enabled, "{password_login:?}");
            let env = Env::with(cfg, Vec::new());
            let (_, _, b) = env.call("GET", SESSION_ROUTE, None, None).await;
            assert_eq!(b["password"], enabled);
            if enabled {
                continue;
            }
            let scope = env.account("admin@example.org").await;
            add_password(&env, scope, "admin@example.org", "s3cret", &[]);
            let body = password_body("admin@example.org", "s3cret");
            let (s, _, _) = env.call("POST", SESSION_ROUTE, None, Some(body)).await;
            assert_eq!(s, StatusCode::NOT_FOUND);
            assert!(env.auth.peers().is_empty(), "пароль не проверяется");
            let log = env.audit.records();
            assert_eq!(
                (log[0].status, log[0].actor_login.as_str()),
                (404, "admin@example.org")
            );
        }
        let c: crate::server::AdminConfig =
            serde_json::from_value(json!({"password_login": false})).expect("config");
        assert_eq!(c.password_login, Some(false));
    }

    /// Пароль проверяется с адресом клиента соединения (по нему — защита от перебора узла); тело не по схеме — 400.
    #[tokio::test]
    async fn password_login_passes_peer_and_checks_body() {
        let env = Env::new();
        let peer = std::net::SocketAddr::from(([192, 0, 2, 7], 50_000));
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(SESSION_ROUTE)
            .header(header::CONTENT_TYPE, "application/json")
            .extension(ConnectInfo(peer))
            .body(axum::body::Body::from(
                password_body("admin@example.org", "nope").to_string(),
            ))
            .expect("request");
        let resp = tower::ServiceExt::oneshot(env.router.clone(), req)
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(env.auth.peers(), vec![Some(peer.ip())]);

        for bad in [
            json!({"login": "admin@example.org"}),
            json!({"login": "a@b.test", "password": "x", "otp": "1"}),
            json!({"login": " ", "password": "x"}),
            json!({"login": "a@b.test", "password": ""}),
            json!({"login": "a".repeat(400), "password": "x"}),
        ] {
            let (s, _, b) = env
                .call("POST", SESSION_ROUTE, None, Some(bad.clone()))
                .await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {b}");
        }
        assert_eq!(
            env.auth.peers().len(),
            1,
            "тело не по схеме — пароль не проверяется"
        );
        let log = env.audit.records();
        assert!(log[1..].iter().all(|r| r.status == 400), "{log:?}");
        assert!(
            log.iter()
                .all(|r| r.actor_login.chars().count() <= MAX_LOGIN_CHARS + 1)
        );

        let spec = serde_json::to_value(crate::server::openapi_spec()).expect("spec");
        let post = &spec["paths"][SESSION_ROUTE]["post"];
        assert_eq!(post["security"], json!([{}, {"bearer": []}]));
        // Тело необязательно: без него — обмен токена `admin init`.
        assert!(post["requestBody"].get("required").is_none(), "{post}");
        assert_eq!(
            post["requestBody"]["content"]["application/json"]["schema"]["oneOf"][0]["$ref"],
            "#/components/schemas/PasswordLogin"
        );
        assert!(
            !format!(
                "{:?}",
                PasswordLogin {
                    login: "a".into(),
                    password: "hidden".into()
                }
            )
            .contains("hidden")
        );
    }

    /// Приёмка ADM-25: способы входа — без заголовка `Authorization`; с настройкой OIDC и без. Издатель — из
    /// `auth.oidc` (`Deps::oidc_issuer`), клиент — из `admin.oidc`.
    #[tokio::test]
    async fn session_info_without_login() {
        let env = Env::new();
        let (s, _, b) = env.call("GET", SESSION_ROUTE, None, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, json!({"oidc": null, "password": true}));

        let oidc = OidcClient {
            client_id: "thepost-console".into(),
            scope: None,
        };
        let env = Env::with(
            crate::server::AdminConfig {
                oidc: Some(oidc),
                ..crate::testenv::config()
            },
            Vec::new(),
        );
        let (s, _, b) = env.call("GET", SESSION_ROUTE, None, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            b,
            json!({
                "oidc": {"issuer": crate::testenv::ISSUER, "client_id": "thepost-console"},
                "password": false
            })
        );
        let spec = serde_json::to_value(crate::server::openapi_spec()).expect("spec");
        assert_eq!(spec["paths"][SESSION_ROUTE]["get"]["security"], json!([{}]));
    }

    #[test]
    fn oidc_client_check() {
        let ok = OidcClient {
            client_id: "c".into(),
            scope: Some("openid profile".into()),
        };
        assert!(ok.check().is_ok());
        for bad in [
            OidcClient {
                client_id: " ".into(),
                ..ok.clone()
            },
            OidcClient {
                scope: Some(String::new()),
                ..ok.clone()
            },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        // Издатель задаётся в `auth.oidc`, не здесь.
        let e = serde_json::from_value::<OidcClient>(
            json!({"issuer": "https://idp.example.org", "client_id": "c"}),
        );
        assert!(e.is_err());
    }

    /// `admin.oidc` без `auth.oidc` (издателя нет) — ошибка конфигурации при запуске.
    #[test]
    fn oidc_client_requires_issuer() {
        let cfg = crate::server::AdminConfig {
            oidc: Some(OidcClient {
                client_id: "thepost-console".into(),
                scope: None,
            }),
            ..crate::testenv::config()
        };
        let d = crate::server::Deps {
            oidc_issuer: None,
            ..crate::testenv::deps()
        };
        let e = Admin::new(d, cfg).map(|_| ()).expect_err("no issuer");
        assert!(
            matches!(&e, Error::Invalid(m) if m.contains("auth.oidc")),
            "{e:?}"
        );
    }

    /// Приёмка ADM-11: init заводит домен, учётную запись с паролем и токен; токен обменивается на сессию один раз.
    #[tokio::test]
    async fn init_then_exchange_once() {
        let env = Env::new();
        let g = env
            .admin
            .init(&req(" Boss@Example.Test ", false))
            .await
            .expect("init");
        assert_eq!(g.login, "boss@example.test");
        assert!(g.token.starts_with(LOCAL_ADMIN_TOKEN_PREFIX), "{}", g.token);
        assert!(!format!("{g:?}").contains(&g.token));
        assert!(!format!("{g:?}").contains(&g.password));

        let c = env.transport.cluster().await.expect("cluster");
        let d = c.domain("example.test").expect("принятый домен");
        assert_eq!(
            (d.tenant, &d.kind),
            (TenantId::DEFAULT, &DomainKind::Authoritative)
        );
        let acc = env
            .meta
            .get(TenantId::DEFAULT, g.account)
            .await
            .expect("get")
            .expect("account");
        assert_eq!(
            (acc.domain, acc.enabled, acc.external_id),
            (d.id, true, None)
        );
        let scope = AccountScope {
            tenant: TenantId::DEFAULT,
            account: g.account,
        };
        assert!(
            env.app_passwords
                .verify(scope, &g.password)
                .await
                .expect("verify")
        );
        let tokens = env.tokens.list(TenantId::DEFAULT).await.expect("list");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].hash, hash(&g.token));
        assert_eq!(tokens[0].expires_at, g.expires_at);

        // Токен на обычном маршруте не принимается: только обмен.
        let (s, _, _) = env
            .call("GET", "/api/v1/whoami", Some(&g.token), None)
            .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, h, b) = env.call("POST", SESSION_ROUTE, Some(&g.token), None).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert_eq!(b["login"], "boss@example.test");
        assert_eq!(b["roles"], json!(["installation_admin"]));
        let session = b["session"].as_str().expect("session").to_owned();
        assert!(session.starts_with(SESSION_PREFIX));
        let (s, _, _) = env.call("POST", SESSION_ROUTE, Some(&g.token), None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "токен одноразовый");

        let (s, _, b) = env
            .call("GET", "/api/v1/whoami", Some(&session), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["login"], "boss@example.test");
        assert_eq!(b["roles"], json!(["installation_admin"]));
        let (s, _, _) = env
            .call("GET", "/api/v1/whoami", Some("tps_nope"), None)
            .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, _, _) = env.call("POST", SESSION_ROUTE, Some(&session), None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "сессия — не токен обмена");

        let log = env.audit.records();
        let routes: Vec<_> = log.iter().map(|r| (r.route.as_str(), r.status)).collect();
        assert_eq!(
            routes,
            vec![
                (INIT_ROUTE, 201),
                (SESSION_ROUTE, 201),
                (SESSION_ROUTE, 401),
                (SESSION_ROUTE, 401)
            ]
        );
        assert_eq!(log[1].actor, g.account);
    }

    #[tokio::test]
    async fn repeat_needs_force() {
        let env = Env::new();
        let first = env
            .admin
            .init(&req("boss@example.test", false))
            .await
            .expect("init");
        assert_eq!(
            env.admin
                .init(&req("boss@example.test", false))
                .await
                .map(|_| ()),
            Err(Error::Conflict)
        );
        assert_eq!(
            env.admin
                .init(&req("other@example.test", false))
                .await
                .map(|_| ()),
            Err(Error::Conflict),
            "токен уже выпускался"
        );
        let again = env
            .admin
            .init(&req("boss@example.test", true))
            .await
            .expect("force");
        assert_eq!(again.account, first.account);
        let scope = AccountScope {
            tenant: TenantId::DEFAULT,
            account: first.account,
        };
        assert!(
            !env.app_passwords
                .verify(scope, &first.password)
                .await
                .expect("verify")
        );
        assert!(
            env.app_passwords
                .verify(scope, &again.password)
                .await
                .expect("verify")
        );
        let paths: Vec<_> = env
            .audit
            .records()
            .iter()
            .map(|r| (r.path.clone(), r.status))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("admin init".to_owned(), 201),
                ("admin init".to_owned(), 409),
                ("admin init".to_owned(), 409),
                ("admin init --force".to_owned(), 201)
            ]
        );
        // Пароль `admin init` — пароль учётной записи для протоколов.
        let pw = tp_auth::PasswordAuthenticator::new(tp_auth::Accounts::new(
            TenantId::DEFAULT,
            Arc::new(env.meta.clone()),
        ))
        .with_app_passwords(env.app_passwords.clone());
        let p = tp_contracts::auth::Authenticator::verify(
            &pw,
            Credentials::Password {
                login: "boss@example.test".into(),
                secret: again.password.clone(),
            },
            None,
        )
        .await
        .expect("login");
        assert_eq!(p.account, first.account);
    }

    #[tokio::test]
    async fn bad_address_and_foreign_domain() {
        let env = Env::new();
        for bad in [
            "boss",
            "@example.test",
            "bo ss@example.test",
            "boss@пример.рф",
        ] {
            let r = env.admin.init(&req(bad, false)).await;
            assert!(matches!(r, Err(Error::Invalid(_))), "{bad}: {r:?}");
        }
        let c = env.transport.cluster().await.expect("cluster");
        let foreign = AcceptedDomain {
            id: DomainId::new(),
            tenant: env.other_tenant,
            name: "other.test".into(),
            kind: DomainKind::Authoritative,
        };
        env.transport
            .put_domain(c.version, &foreign)
            .await
            .expect("put");
        let r = env.admin.init(&req("boss@other.test", false)).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    }

    #[tokio::test]
    async fn not_ready_without_stores() {
        let env = Env::without_local();
        assert_eq!(
            env.admin
                .init(&req("boss@example.test", false))
                .await
                .map(|_| ()),
            Err(Error::UNAVAILABLE)
        );
        let (s, _, _) = env.call("POST", SESSION_ROUTE, Some("tpa_x"), None).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        let body = json!({"login": "admin@example.org", "password": "x"});
        let (s, _, _) = env.call("POST", SESSION_ROUTE, None, Some(body)).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn session_expires_and_is_bounded() {
        let env = Env::new();
        let local = env.admin.local.clone().expect("local");
        let now = OffsetDateTime::now_utc();
        let p = crate::testenv::subject(TenantId::DEFAULT, "boss@example.test", &[]);
        let roles = BTreeSet::from([AdminRole::InstallationAdmin]);
        let (v, exp) = env
            .admin
            .open_session(&local, p.clone(), roles.clone(), now)
            .expect("open");
        assert!(env.admin.session_caller(&v, now).expect("session").is_ok());
        assert_eq!(
            env.admin
                .session_caller(&v, exp)
                .expect("session")
                .map(|_| ()),
            Err(Error::Forbidden)
        );
        assert!(env.admin.session_caller("eyJ.x.y", now).is_none());
        let max = env.admin.cfg.init.max_sessions;
        for _ in 0..max + 3 {
            env.admin
                .open_session(&local, p.clone(), roles.clone(), now)
                .expect("open");
        }
        assert_eq!(local.sessions().len(), max);
    }

    /// Локальный сокет: только root; запрос и ответ — строки JSON.
    #[tokio::test]
    async fn socket_root_only() {
        let env = Env::new();
        let call = |uid: Option<u32>, body: Vec<u8>| {
            let admin = env.admin.clone();
            async move {
                let (client, server) = tokio::io::duplex(8192);
                let srv =
                    tokio::spawn(async move { serve_init(&admin, server, uid, Some(65532)).await });
                let (mut r, mut w) = tokio::io::split(client);
                // Длинный запрос сервер дочитывает не до конца и закрывает соединение.
                let _ = w.write_all(&body).await;
                let mut out = Vec::new();
                r.read_to_end(&mut out).await.expect("read");
                srv.await.expect("join").expect("serve");
                serde_json::from_slice::<InitReply>(&out).expect("reply")
            }
        };
        let line = |r: &InitRequest| {
            let mut v = serde_json::to_vec(r).expect("json");
            v.push(b'\n');
            v
        };
        let r = call(Some(1000), line(&req("boss@example.test", false))).await;
        assert!(matches!(r, InitReply::Refused { code: 403, .. }), "{r:?}");
        let r = call(None, line(&req("boss@example.test", false))).await;
        assert!(matches!(r, InitReply::Refused { code: 403, .. }), "{r:?}");
        let r = call(
            Some(65532),
            b"{}
"
            .to_vec(),
        )
        .await;
        assert!(
            matches!(r, InitReply::Refused { code: 400, .. }),
            "служба: {r:?}"
        );
        let r = call(Some(0), b"{\"mail\": 1}\n".to_vec()).await;
        assert!(matches!(r, InitReply::Refused { code: 400, .. }), "{r:?}");
        let r = call(Some(0), vec![b'x'; 10_000]).await;
        assert!(matches!(r, InitReply::Refused { code: 400, .. }), "{r:?}");
        let r = call(Some(0), line(&req("boss@example.test", false))).await;
        let InitReply::Granted(g) = r else {
            panic!("{r:?}")
        };
        assert_eq!(g.login, "boss@example.test");
        let r = call(Some(0), line(&req("boss@example.test", false))).await;
        assert!(matches!(r, InitReply::Refused { code: 409, .. }), "{r:?}");

        // Клиент.
        let (client, server) = tokio::io::duplex(8192);
        let admin = env.admin.clone();
        let srv = tokio::spawn(async move { serve_init(&admin, server, Some(0), None).await });
        let r = request(client, &req("boss@example.test", true), 8192)
            .await
            .expect("request");
        srv.await.expect("join").expect("serve");
        assert!(matches!(r, InitReply::Granted(_)), "{r:?}");
        let audit = env.audit.records();
        assert_eq!(audit[0].status, 403);
        assert_eq!(audit[0].route, INIT_ROUTE);
    }

    #[test]
    fn config() {
        let c: InitConfig = serde_json::from_value(json!({"token_ttl": "5m"})).expect("config");
        assert_eq!(c.token_ttl, Duration::from_secs(300));
        assert_eq!(c.session_ttl, InitConfig::default().session_ttl);
        c.check().expect("check");
        let zero: InitConfig =
            serde_json::from_value(json!({"session_ttl": "0s"})).expect("config");
        assert!(zero.check().is_err());
        assert!(serde_json::from_value::<InitConfig>(json!({"ttl": "5m"})).is_err());
    }

    #[test]
    fn random_values() {
        let a = random_value("tpa_").expect("random");
        let b = random_value("tpa_").expect("random");
        assert_ne!(a, b);
        assert_eq!(a.len(), 4 + 64);
        assert_eq!(hash("abc").len(), 64);
    }
}
