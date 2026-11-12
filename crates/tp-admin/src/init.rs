//! Первый администратор без OIDC: `thepost admin init` (ADM-11, решение владельца #193; CONTRACTS.md#Auth —
//! локальный токен Admin API).
//!
//! - [`serve_init`] — обработчик одного соединения локального сокета узла: запрос — одна строка JSON
//!   ([`InitRequest`]), ответ — одна строка JSON ([`InitReply`]). Принимается только от root или учётной записи
//!   службы (`uid` собеседника по `SO_PEERCRED`; в контейнере без capabilities root не откроет сокет `0600`
//!   службы, поэтому `docker compose exec thepost thepost admin init` идёт от учётной записи службы). Сокет
//!   (`admin.init.socket`) и права на него — у вызывающего, `thepost`; [`serve_local`] — цикл приёма на Unix;
//!   клиент — [`request`].
//! - [`Admin::init`] — организация по умолчанию, принятый домен адреса (создаётся, если нет), учётная запись
//!   администратора, пароль (пароль приложения: вход IMAP, SMTP, JMAP) и одноразовый токен Admin API со сроком
//!   `admin.init.token_ttl`. Администратор уже заведён (есть учётная запись с этим адресом или ранее выпущенный
//!   токен) — отказ, `force` — повтор (прежние пароли `admin init` отзываются). Каждый вызов — в журнал аудита.
//! - `POST /api/v1/session` — обмен токена на сессию Admin API (решение 2026-11-11, п. 2): токен гасится первой
//!   проверкой, в том же ответе — значение сессии (`tps_…`, срок `admin.init.session_ttl`); дальше — только сессия
//!   или OIDC Bearer. Роль сессии — администратор инсталляции. Сессии — в памяти узла: после перезапуска —
//!   повторный `admin init --force` или вход через OIDC.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tp_contracts::admin_audit::AdminAuditRecord;
use tp_contracts::admin_token::{
    LOCAL_ADMIN_TOKEN_PREFIX, LocalAdminToken, LocalAdminTokens, is_local_admin_token,
};
use tp_contracts::auth::Principal;
use tp_contracts::config::duration;
use tp_contracts::meta::{Account, AccountScope, AccountUpsert};
use tp_contracts::transport::{AcceptedDomain, DomainKind};
use tp_contracts::{AccountId, DomainId, Error, Result, TenantId};
use utoipa::{OpenApi, ToSchema};

use crate::server::{Admin, AdminRole, ApiError, Caller, Problem};

/// Маршрут обмена локального токена на сессию.
pub const SESSION_ROUTE: &str = "/api/v1/session";

/// Префикс значения сессии Admin API в `Authorization: Bearer`.
pub const SESSION_PREFIX: &str = "tps_";

/// Подпись паролей, выпущенных `admin init`.
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
                    },
                )
                .await?
            }
        };
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

/// Маршрут обмена (вне слоя доступа: вход — сам локальный токен).
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route(SESSION_ROUTE, post(exchange))
}

/// Спецификация OpenAPI маршрута обмена.
#[derive(Debug, OpenApi)]
#[openapi(paths(exchange), components(schemas(AdminSession)))]
pub(crate) struct Doc;

/// Обменивает одноразовый токен `admin init` (`Authorization: Bearer tpa_…`) на сессию Admin API. Токен гасится
/// первой проверкой; повтор, срок, отзыв — 401.
#[utoipa::path(post, path = "/api/v1/session", tag = "admin",
    responses((status = 201, body = AdminSession), (status = 401, body = Problem), (status = 503, body = Problem)))]
async fn exchange(State(a): State<Arc<Admin>>, headers: HeaderMap) -> Response {
    let Some(local) = a.local.clone() else {
        return ApiError(Error::UNAVAILABLE).into_response();
    };
    let now = OffsetDateTime::now_utc();
    let r = exchange_inner(&a, &local, &headers, now).await;
    let (actor, login, roles, status) = match &r {
        Ok(s) => (
            s.account.parse().unwrap_or_else(|_| nobody()),
            s.login.clone(),
            s.roles.iter().map(|r| r.as_str().to_owned()).collect(),
            201,
        ),
        Err(e) if e.is_retryable() => (nobody(), String::new(), Vec::new(), 503),
        Err(_) => (nobody(), String::new(), Vec::new(), 401),
    };
    let record = AdminAuditRecord {
        at: now,
        tenant: Some(TenantId::DEFAULT),
        actor_tenant: TenantId::DEFAULT,
        actor,
        actor_login: login,
        roles,
        method: "POST".into(),
        route: SESSION_ROUTE.into(),
        path: SESSION_ROUTE.into(),
        status,
        content_messages: None,
    };
    if let Err(e) = crate::audit::record(&a.d, &record).await {
        tracing::error!(error = %e, route = SESSION_ROUTE, "admin: запись аудита не сохранена");
    }
    match r {
        Ok(s) => {
            let mut resp = (StatusCode::CREATED, Json(s)).into_response();
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err(e) => crate::server::auth_error(&e),
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
