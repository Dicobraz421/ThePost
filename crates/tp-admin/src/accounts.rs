//! Организация, домены, учётные записи и квоты (SRS 5.2, 3.8).
//!
//! - организация (`/api/v1/orgs/{org}`): чтение и политика уровня организации;
//! - домены организации — принятые домены транспорта и политика домена ([`crate::transport`], CTR-24);
//! - учётные записи приходят из каталога (LDAP): здесь — поиск по имени входа, постраничный список (по возрастанию
//!   имени входа, фильтр по префиксу; продолжение — заголовок `Link` с `rel="next"`, RFC 8288), чтение с
//!   использованием квоты и только локальные атрибуты — политика ящика (квота, срок корзины, лимиты отправки);
//!   без каталога (SMB, решение #193) — создание локальной учётной записи в принятом домене организации (пароль — пароль приложения);
//!   изменение (`PATCH`): отображаемое имя и блокировка входа (`enabled`) — только у локальных записей, у записей
//!   каталога оба поля ведёт каталог (409);
//! - правило `trash.purge` (`mode`, `user_can_choose`) — поле политики уровня; выбор пользователя и действующее
//!   значение ящика — [`crate::retention`];
//! - политика уровня задаётся целиком (`PUT`), поле `null` — не задано на уровне; действующая политика
//!   (`effective`) — наложение цепочки организация → домен → ящик (`PolicyResolver`).
//!
//! Записи политик в контрактах пока нет: [`PolicyStore`] — временный трейт полосы (как `AppPasswordStore` в AUTH-02
//! до CTR-22), повторяет `PgMeta::policy`/`set_policy`.
//! Список учётных записей — `AccountStore::list` (CTR-44).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tp_contracts::config::duration;
use tp_contracts::meta::{Account, AccountUpsert, Policy};
use tp_contracts::retention::TrashPurgeRule;
use tp_contracts::transport::normalize_domain;
use tp_contracts::{AccountId, DomainId, Error, Result, TenantId};
use utoipa::{OpenApi, ToSchema};

use crate::retention::TrashPurgeRuleBody;
use crate::server::{Admin, ApiResult, Problem, Refused};

/// Уровень цепочки политик, задаваемый через Admin API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyTarget {
    /// Организация.
    Tenant,
    /// Домен.
    Domain(DomainId),
    /// Учётная запись (ящик).
    Account(AccountId),
}

/// Политики уровней цепочки (временный трейт до контракта).
#[async_trait]
pub trait PolicyStore: Send + Sync + 'static {
    /// Политика одного уровня без наложения; не задана — все поля `None`.
    async fn policy(&self, tenant: TenantId, target: PolicyTarget) -> Result<Policy>;

    /// Задаёт политику уровня целиком; политика без заданных полей удаляет уровень.
    async fn set_policy(
        &self,
        tenant: TenantId,
        target: PolicyTarget,
        policy: &Policy,
    ) -> Result<()>;
}

/// Политика уровня; `null` — не задано на уровне.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyBody {
    /// Квота ящика, байт.
    #[serde(default)]
    pub quota_bytes: Option<u64>,
    /// Срок хранения в корзине (`30d`, `12h`).
    #[serde(default, with = "duration::option")]
    #[schema(value_type = Option<String>, example = "30d")]
    pub trash_retention: Option<Duration>,
    /// Срок хранения писем (политика хранения).
    #[serde(default, with = "duration::option")]
    #[schema(value_type = Option<String>, example = "365d")]
    pub retention: Option<Duration>,
    /// Лимит отправки, писем в час.
    #[serde(default)]
    pub send_per_hour: Option<u32>,
    /// Лимит отправки, адресатов в час (сумма получателей писем за скользящий час).
    #[serde(default)]
    pub rcpts_per_hour: Option<u32>,
    /// Лимит получателей одного письма.
    #[serde(default)]
    pub max_recipients: Option<u32>,
    /// Селектор ключа DKIM.
    #[serde(default)]
    pub dkim_selector: Option<String>,
    /// Профиль Milter.
    #[serde(default)]
    pub milter_profile: Option<String>,
    /// Правило «очистить корзину» в клиенте (`trash.purge`, решение 2026-11-01-trash-purge).
    #[serde(default)]
    pub trash_purge: Option<TrashPurgeRuleBody>,
}

impl From<Policy> for PolicyBody {
    fn from(p: Policy) -> Self {
        Self {
            quota_bytes: p.quota_bytes,
            trash_retention: p.trash_retention,
            retention: p.retention,
            send_per_hour: p.send_per_hour,
            rcpts_per_hour: p.rcpts_per_hour,
            max_recipients: p.max_recipients,
            dkim_selector: p.dkim_selector,
            milter_profile: p.milter_profile,
            trash_purge: p.trash_purge.map(TrashPurgeRuleBody::from),
        }
    }
}

impl From<PolicyBody> for Policy {
    fn from(p: PolicyBody) -> Self {
        Self {
            quota_bytes: p.quota_bytes,
            trash_retention: p.trash_retention,
            retention: p.retention,
            send_per_hour: p.send_per_hour,
            rcpts_per_hour: p.rcpts_per_hour,
            max_recipients: p.max_recipients,
            dkim_selector: p.dkim_selector,
            milter_profile: p.milter_profile,
            trash_purge: p.trash_purge.map(TrashPurgeRule::from),
        }
    }
}

/// Политика уровня и действующая политика.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Policies {
    /// Заданное на этом уровне.
    pub policy: PolicyBody,
    /// Действующее с учётом вышестоящих уровней; `null` — значение по умолчанию конфигурации.
    pub effective: PolicyBody,
}

/// Организация.
#[derive(Debug, Serialize, ToSchema)]
pub struct Organization {
    /// Идентификатор.
    pub id: String,
    /// Организация по умолчанию.
    pub default: bool,
    /// Политики.
    #[serde(flatten)]
    pub policies: Policies,
}

/// Использование квоты.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QuotaInfo {
    /// Занято байт, включая корзину.
    pub used_bytes: u64,
    /// Число писем.
    pub messages: u64,
    /// Лимит; `null` — без ограничения.
    pub limit_bytes: Option<u64>,
}

/// Учётная запись.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AccountInfo {
    /// Идентификатор.
    pub id: String,
    /// Основной домен.
    pub domain: String,
    /// Имя входа.
    pub login: String,
    /// Отображаемое имя.
    pub display_name: String,
    /// Идентификатор в каталоге; `null` — локальная запись.
    pub external_id: Option<String>,
    /// Вход разрешён.
    pub enabled: bool,
    /// Квота.
    pub quota: QuotaInfo,
    /// Политики ящика.
    #[serde(flatten)]
    pub policies: Policies,
}

/// Новая локальная учётная запись (без каталога).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewAccount {
    /// Имя входа — адрес в принятом домене организации (приводится к нижнему регистру).
    pub login: String,
    /// Отображаемое имя; нет — имя входа.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Изменение учётной записи: заданные поля заменяются, отсутствующие — без изменений; хотя бы одно поле обязательно.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountPatch {
    /// Отображаемое имя (непустое, одна строка); у записи каталога — 409.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Вход разрешён; `false` — учётная запись заблокирована. У записи каталога — 409.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Поиск по имени входа или страница списка учётных записей.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub struct AccountQuery {
    /// Имя входа (без учёта регистра): поиск одной записи; несовместимо с `prefix`, `after`, `limit`.
    #[serde(default)]
    pub login: Option<String>,
    /// Список: имя входа начинается с префикса (без учёта регистра).
    #[serde(default)]
    pub prefix: Option<String>,
    /// Список: продолжение после этого имени входа — `after` из ссылки `rel="next"` предыдущей страницы.
    #[serde(default)]
    pub after: Option<String>,
    /// Список: размер страницы (по умолчанию и наибольший — `admin.page_size`, `admin.max_page_size`).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Маршруты организации и учётных записей.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route("/api/v1/orgs/{org}", get(get_org))
        .route("/api/v1/orgs/{org}/policy", put(put_org_policy))
        .route(
            "/api/v1/orgs/{org}/accounts",
            get(list_accounts).post(create_account),
        )
        .route(
            "/api/v1/orgs/{org}/accounts/{account}",
            get(get_account).patch(patch_account),
        )
        .route(
            "/api/v1/orgs/{org}/accounts/{account}/policy",
            put(put_account_policy),
        )
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(
        get_org,
        put_org_policy,
        list_accounts,
        create_account,
        get_account,
        patch_account,
        put_account_policy
    ),
    components(schemas(
        PolicyBody,
        Policies,
        Organization,
        QuotaInfo,
        AccountInfo,
        NewAccount,
        AccountPatch
    ))
)]
pub(crate) struct Doc;

/// Учётная запись организации; нет — `NotFound`.
pub(crate) async fn account(a: &Admin, org: TenantId, id: AccountId) -> Result<Account> {
    a.d.accounts.get(org, id).await?.ok_or(Error::NotFound)
}

/// Политика уровня и действующая политика (цепочка организация → домен → ящик).
pub(crate) async fn policies(
    a: &Admin,
    org: TenantId,
    target: PolicyTarget,
    domain: Option<DomainId>,
    account: Option<AccountId>,
) -> Result<Policies> {
    Ok(Policies {
        policy: a.d.policies.policy(org, target).await?.into(),
        effective: a
            .d
            .resolver
            .resolve(org, domain, account, None)
            .await?
            .into(),
    })
}

/// Имя входа (адрес) в нижнем регистре и его домен в канонической форме; не `имя@домен` — `Invalid`.
pub(crate) fn split_login(login: &str) -> Result<(String, String)> {
    let login = login.trim().to_ascii_lowercase();
    match login.rsplit_once('@') {
        Some((user, domain))
            if !user.is_empty() && !login.chars().any(|c| c.is_whitespace() || c.is_control()) =>
        {
            let domain = normalize_domain(domain)?;
            Ok((login, domain))
        }
        _ => Err(Error::invalid(format!(
            "имя входа {login:?} — не адрес вида имя@домен"
        ))),
    }
}

/// Организация и её политика.
#[utoipa::path(get, path = "/api/v1/orgs/{org}", tag = "organizations",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = Organization), (status = 403, body = Problem)))]
async fn get_org(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<Organization>> {
    Ok(Json(Organization {
        id: org.to_string(),
        default: org == TenantId::DEFAULT,
        policies: policies(&a, org, PolicyTarget::Tenant, None, None).await?,
    }))
}

/// Задаёт политику организации целиком.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/policy", tag = "organizations",
    params(("org" = String, Path, description = "Организация (UUID)")),
    request_body = PolicyBody,
    responses((status = 200, body = PolicyBody), (status = 400, body = Problem), (status = 403, body = Problem)))]
async fn put_org_policy(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Json(body): Json<PolicyBody>,
) -> ApiResult<Json<PolicyBody>> {
    a.d.policies
        .set_policy(org, PolicyTarget::Tenant, &body.clone().into())
        .await?;
    Ok(Json(body))
}

/// Учётные записи. С `login` — поиск: список из одной записи или пустой. Без `login` — страница по возрастанию
/// имени входа (фильтр `prefix`); есть продолжение — заголовок `Link: <…&after=…>; rel="next"` (RFC 8288), нет
/// заголовка — страница последняя. `login` вместе с параметрами списка — 400.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts", tag = "accounts",
    params(("org" = String, Path, description = "Организация (UUID)"), AccountQuery),
    responses(
        (status = 200, body = Vec<AccountInfo>, headers(("Link" = String,
            description = "Следующая страница списка: `<ссылка>; rel=\"next\"`; нет — страница последняя"))),
        (status = 400, body = Problem), (status = 403, body = Problem)))]
async fn list_accounts(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Query(q): Query<AccountQuery>,
) -> ApiResult<(HeaderMap, Json<Vec<AccountInfo>>)> {
    let mut out = Vec::new();
    let mut headers = HeaderMap::new();
    if let Some(login) = &q.login {
        if q.prefix.is_some() || q.after.is_some() || q.limit.is_some() {
            return Err(Error::invalid(
                "login (поиск) несовместим с prefix, after, limit (список)",
            )
            .into());
        }
        if let Some(acc) = a.d.accounts.by_login(org, login).await? {
            out.push(account_info(&a, acc).await?);
        }
        return Ok((headers, Json(out)));
    }
    let limit = a.cfg.page(q.limit);
    let prefix = q
        .prefix
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let after = q.after.as_deref().map(str::to_lowercase);
    // На одну запись больше: продолжение есть, только если после страницы записи действительно есть.
    let mut page =
        a.d.accounts
            .list(org, &prefix, after.as_deref(), limit.saturating_add(1))
            .await?;
    if page.len() > limit {
        page.truncate(limit);
        if let Some(last) = page.last() {
            let link = next_link(org, &prefix, &last.login.to_lowercase(), limit);
            let value = HeaderValue::from_str(&link)
                .map_err(|e| Error::internal(format!("ссылка Link: {e}")))?;
            headers.insert(header::LINK, value);
        }
    }
    for acc in page {
        out.push(account_info(&a, acc).await?);
    }
    Ok((headers, Json(out)))
}

/// Ссылка на следующую страницу списка (RFC 8288; путь от корня Admin API).
fn next_link(org: TenantId, prefix: &str, after: &str, limit: usize) -> String {
    let mut q = String::new();
    if !prefix.is_empty() {
        q.push_str("prefix=");
        q.push_str(&encode_query(prefix));
        q.push('&');
    }
    format!(
        "</api/v1/orgs/{org}/accounts?{q}limit={limit}&after={}>; rel=\"next\"",
        encode_query(after)
    )
}

/// Значение параметра запроса: всё, кроме незарезервированных символов и `@` (RFC 3986), — `%XX`.
fn encode_query(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'@') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Создаёт локальную учётную запись (без каталога) в принятом домене организации. Имя занято — 409; домен адреса
/// не принят организацией — 400; настройки транспорта не подключены — 503. Пароль — пароль приложения
/// (`POST .../app-passwords`).
#[utoipa::path(post, path = "/api/v1/orgs/{org}/accounts", tag = "accounts",
    params(("org" = String, Path, description = "Организация (UUID)")),
    request_body = NewAccount,
    responses((status = 201, body = AccountInfo), (status = 400, body = Problem), (status = 409, body = Problem),
        (status = 503, body = Problem)))]
async fn create_account(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Json(body): Json<NewAccount>,
) -> ApiResult<(StatusCode, Json<AccountInfo>)> {
    let (login, domain) = split_login(&body.login)?;
    let t = a.transport.as_deref().ok_or(Error::UNAVAILABLE)?;
    let snap = t.cluster().await?;
    let d = snap
        .domain(&domain)
        .filter(|d| d.tenant == org)
        .ok_or_else(|| {
            Error::invalid(format!(
                "домен {domain} не принят организацией: сначала POST /api/v1/orgs/{org}/transport/domains"
            ))
        })?;
    if a.d.accounts.by_login(org, &login).await?.is_some() {
        return Err(Error::Conflict.into());
    }
    let display_name = body
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(&login)
        .to_owned();
    let acc =
        a.d.accounts
            .upsert(
                org,
                AccountUpsert {
                    domain: d.id,
                    login,
                    display_name,
                    external_id: None,
                    enabled: true,
                },
            )
            .await?;
    Ok((StatusCode::CREATED, Json(account_info(&a, acc).await?)))
}

async fn account_info(a: &Admin, acc: Account) -> Result<AccountInfo> {
    let scope = tp_contracts::meta::AccountScope {
        tenant: acc.tenant,
        account: acc.id,
    };
    let q = a.d.quota.usage(scope).await?;
    let policies = policies(
        a,
        acc.tenant,
        PolicyTarget::Account(acc.id),
        Some(acc.domain),
        Some(acc.id),
    )
    .await?;
    Ok(AccountInfo {
        id: acc.id.to_string(),
        domain: acc.domain.to_string(),
        login: acc.login,
        display_name: acc.display_name,
        external_id: acc.external_id,
        enabled: acc.enabled,
        quota: QuotaInfo {
            used_bytes: q.used_bytes,
            messages: q.messages,
            limit_bytes: q.limit_bytes,
        },
        policies,
    })
}

/// Учётная запись: квота и политики ящика.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}", tag = "accounts",
    params(("org" = String, Path, description = "Организация (UUID)"), ("account" = String, Path, description = "Учётная запись (UUID)")),
    responses((status = 200, body = AccountInfo), (status = 404, body = Problem)))]
async fn get_account(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<Json<AccountInfo>> {
    let acc = account(&a, org, id).await?;
    Ok(Json(account_info(&a, acc).await?))
}

/// Изменяет локальную учётную запись: отображаемое имя и блокировка входа (`enabled: false` — вход запрещён).
/// Пустое тело (`{}`) — 422. У записи каталога (`external_id` задан) оба поля ведёт каталог: синхронизация
/// перезаписывает и имя, и признак блокировки, поэтому изменение — 409 с пояснением.
#[utoipa::path(patch, path = "/api/v1/orgs/{org}/accounts/{account}", tag = "accounts",
    params(("org" = String, Path, description = "Организация (UUID)"), ("account" = String, Path, description = "Учётная запись (UUID)")),
    request_body = AccountPatch,
    responses((status = 200, body = AccountInfo), (status = 400, body = Problem), (status = 404, body = Problem),
        (status = 409, body = Problem), (status = 422, body = Problem)))]
async fn patch_account(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
    Json(p): Json<AccountPatch>,
) -> std::result::Result<Json<AccountInfo>, Refused> {
    if p.display_name.is_none() && p.enabled.is_none() {
        return Err(Refused::Problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "нужно хотя бы одно поле: display_name, enabled".to_owned(),
        ));
    }
    let acc = account(&a, org, id).await?;
    if acc.external_id.is_some() {
        let detail = if p.display_name.is_some() {
            "отображаемое имя записи каталога ведёт каталог: измените его в каталоге"
        } else {
            "признак блокировки записи каталога перезаписывает синхронизация каталога: \
             заблокируйте учётную запись в каталоге"
        };
        return Err(Refused::Problem(StatusCode::CONFLICT, detail.to_owned()));
    }
    let display_name = match p.display_name.as_deref().map(str::trim) {
        Some(n) if n.is_empty() || n.chars().any(char::is_control) => {
            return Err(
                Error::invalid("display_name — непустая строка без управляющих символов").into(),
            );
        }
        Some(n) => n.to_owned(),
        None => acc.display_name,
    };
    let acc =
        a.d.accounts
            .upsert(
                org,
                AccountUpsert {
                    domain: acc.domain,
                    login: acc.login,
                    display_name,
                    external_id: None,
                    enabled: p.enabled.unwrap_or(acc.enabled),
                },
            )
            .await?;
    Ok(Json(account_info(&a, acc).await?))
}

/// Задаёт политику ящика (локальные атрибуты: квота, срок корзины, лимиты) целиком.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/accounts/{account}/policy", tag = "accounts",
    params(("org" = String, Path, description = "Организация (UUID)"), ("account" = String, Path, description = "Учётная запись (UUID)")),
    request_body = PolicyBody,
    responses((status = 200, body = PolicyBody), (status = 404, body = Problem)))]
async fn put_account_policy(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
    Json(body): Json<PolicyBody>,
) -> ApiResult<Json<PolicyBody>> {
    account(&a, org, id).await?;
    a.d.policies
        .set_policy(org, PolicyTarget::Account(id), &body.clone().into())
        .await?;
    Ok(Json(body))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use tp_contracts::TenantId;

    use super::{AccountQuery, encode_query};
    use crate::server::AdminConfig;
    use crate::testenv::{Env, config};

    fn org() -> String {
        format!("/api/v1/orgs/{}", TenantId::DEFAULT)
    }

    #[tokio::test]
    async fn organization_policy() {
        let env = Env::new();
        let (s, _, b) = env.call("GET", &org(), Some("org"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["default"], true);
        assert_eq!(b["policy"]["quota_bytes"], Value::Null);
        let body = json!({"quota_bytes": 1000, "trash_retention": "14d"});
        let policy = format!("{}/policy", org());
        let (s, _, b) = env.call("PUT", &policy, Some("org"), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["trash_retention"], "14d");
        let (_, _, b) = env.call("GET", &org(), Some("org"), None).await;
        assert_eq!(b["policy"]["quota_bytes"], 1000);
        assert_eq!(b["effective"]["trash_retention"], "14d");

        for bad in [json!({"trash_retention": "14 days"}), json!({"quota": 1})] {
            let (s, _, _) = env.call("PUT", &policy, Some("org"), Some(bad)).await;
            assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    /// Администратор организации — только своя организация; администратор инсталляции — любая.
    #[tokio::test]
    async fn organization_scope() {
        let env = Env::new();
        let other = format!("/api/v1/orgs/{}", env.other_tenant);
        let (s, _, _) = env.call("GET", &other, Some("org"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env.call("GET", &other, Some("other"), None).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _, _) = env.call("GET", &org(), Some("other"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, b) = env.call("GET", &other, Some("root"), None).await;
        assert_eq!((s, &b["default"]), (StatusCode::OK, &json!(false)));
        let bad = "/api/v1/orgs/not-a-uuid";
        let (s, _, _) = env.call("GET", bad, Some("root"), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        let policy = format!("{other}/policy");
        let (s, _, _) = env.call("PUT", &policy, Some("org"), Some(json!({}))).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let last = env.audit.records().pop().expect("audit");
        assert_eq!((last.tenant, last.status), (Some(env.other_tenant), 403));
    }

    /// Локальная учётная запись: только в принятом домене организации, имя не занято.
    #[tokio::test]
    async fn create_local_account() {
        use tp_contracts::transport::{AcceptedDomain, DomainKind, TransportStore};

        let env = Env::new();
        let base = format!("{}/accounts", org());
        let new = |login: &str| Some(json!({ "login": login, "display_name": "Анна" }));
        let (s, _, b) = env
            .call("POST", &base, Some("org"), new("a@example.test"))
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        // Домен не принят — подсказка на единственный API доменов организации.
        let detail = b["detail"].as_str().expect("detail");
        assert!(detail.contains("не принят организацией"), "{detail}");
        assert!(detail.contains("/transport/domains"), "{detail}");
        let c = env.transport.cluster().await.expect("cluster");
        let d = AcceptedDomain {
            id: tp_contracts::DomainId::new(),
            tenant: TenantId::DEFAULT,
            name: "example.test".into(),
            kind: DomainKind::Authoritative,
        };
        env.transport
            .put_domain(c.version, &d)
            .await
            .expect("domain");
        let (s, _, _) = env
            .call("POST", &base, Some("user"), new("a@example.test"))
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, b) = env
            .call("POST", &base, Some("org"), new(" A@Example.Test "))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        assert_eq!(b["login"], "a@example.test");
        assert_eq!(b["display_name"], "Анна");
        assert_eq!(b["domain"], d.id.to_string());
        assert_eq!(b["external_id"], Value::Null);
        let (s, _, _) = env
            .call("POST", &base, Some("org"), new("a@example.test"))
            .await;
        assert_eq!(s, StatusCode::CONFLICT);
        for bad in ["a", "a b@example.test", "@example.test"] {
            let (s, _, _) = env.call("POST", &base, Some("org"), new(bad)).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
        }
        let (_, _, list) = env
            .call(
                "GET",
                &format!("{base}?login=a@example.test"),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(list[0]["login"], "a@example.test");

        let other = format!("/api/v1/orgs/{}/accounts", env.other_tenant);
        let (s, _, _) = env
            .call("POST", &other, Some("other"), new("b@example.test"))
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "домен чужой организации");
        let env = Env::without_transport();
        let (s, _, _) = env
            .call("POST", &base, Some("org"), new("a@example.test"))
            .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn accounts_and_quota() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        env.quota.set_limit(scope, Some(5_000));
        let one = format!("{}/accounts/{}", org(), scope.account);
        let (s, _, b) = env.call("GET", &one, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["login"], "user@example.org");
        assert_eq!(b["external_id"], Value::Null);
        let quota = json!({"used_bytes": 0, "messages": 0, "limit_bytes": 5000});
        assert_eq!(b["quota"], quota);

        let find = |login: &str| format!("{}/accounts?login={login}", org());
        let (s, _, list) = env
            .call("GET", &find("USER@example.org"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(list[0]["id"], scope.account.to_string());
        let (_, _, list) = env
            .call("GET", &find("nobody@example.org"), Some("org"), None)
            .await;
        assert_eq!(list, json!([]));

        let opolicy = json!({"quota_bytes": 100, "send_per_hour": 7, "rcpts_per_hour": 900});
        env.call(
            "PUT",
            &format!("{}/policy", org()),
            Some("org"),
            Some(opolicy),
        )
        .await;
        let apolicy = format!("{one}/policy");
        let (s, _, b) = env
            .call(
                "PUT",
                &apolicy,
                Some("org"),
                Some(json!({"quota_bytes": 2048})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (_, _, b) = env.call("GET", &one, Some("org"), None).await;
        assert_eq!(b["policy"]["quota_bytes"], 2048);
        assert_eq!(b["effective"]["quota_bytes"], 2048);
        assert_eq!(b["effective"]["send_per_hour"], 7);
        assert_eq!(b["effective"]["rcpts_per_hour"], 900);

        let missing = format!("{}/accounts/{}", org(), tp_contracts::AccountId::new());
        let (s, _, _) = env.call("GET", &missing, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let mpolicy = format!("{missing}/policy");
        let (s, _, _) = env
            .call("PUT", &mpolicy, Some("org"), Some(json!({})))
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = env.call("GET", &one, Some("user"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    /// Путь следующей страницы из `Link: <путь>; rel="next"`; нет заголовка — `None`.
    fn next(h: &axum::http::HeaderMap) -> Option<String> {
        let v = h.get(axum::http::header::LINK)?.to_str().expect("link");
        let path = v.strip_prefix('<')?.strip_suffix(">; rel=\"next\"")?;
        Some(path.to_owned())
    }

    fn logins(list: &Value) -> Vec<String> {
        list.as_array()
            .expect("list")
            .iter()
            .map(|a| a["login"].as_str().expect("login").to_owned())
            .collect()
    }

    /// Список без `login`: страницы по возрастанию имени входа, продолжение — `Link`, только своя организация.
    #[tokio::test]
    async fn account_pages() {
        let env = Env::new();
        for l in [
            "c@example.org",
            "A@example.org",
            "b+x@example.org",
            "x@example.net",
        ] {
            env.account(l).await;
        }
        env.account_in(env.other_tenant, "z@other.test").await;
        let base = format!("{}/accounts", org());

        let (s, h, b) = env
            .call("GET", &format!("{base}?limit=2"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(logins(&b), ["A@example.org", "b+x@example.org"]);
        assert_eq!(b[0]["quota"]["used_bytes"], 0, "та же запись, что у поиска");
        let link = next(&h).expect("next");
        assert!(link.ends_with("limit=2&after=b%2Bx@example.org"), "{link}");
        let (_, h, b) = env.call("GET", &link, Some("org"), None).await;
        assert_eq!(logins(&b), ["c@example.org", "x@example.net"]);
        assert!(next(&h).is_none(), "страница последняя");
        let after_last = format!("{base}?after=x@example.net");
        let (s, h, b) = env.call("GET", &after_last, Some("org"), None).await;
        assert_eq!(
            (s, b),
            (StatusCode::OK, json!([])),
            "после последней — пусто"
        );
        assert!(next(&h).is_none());

        // Ровно на границе страницы продолжения нет.
        let (_, h, b) = env
            .call("GET", &format!("{base}?limit=4"), Some("org"), None)
            .await;
        assert_eq!(logins(&b).len(), 4);
        assert!(next(&h).is_none());

        // Префикс без учёта регистра; продолжение сохраняет префикс.
        let (_, h, b) = env
            .call("GET", &format!("{base}?prefix=B"), Some("org"), None)
            .await;
        assert_eq!(logins(&b), ["b+x@example.org"]);
        assert!(next(&h).is_none());
        env.account("bob@example.org").await;
        let (_, h, b) = env
            .call(
                "GET",
                &format!("{base}?prefix=b&limit=1"),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(logins(&b), ["b+x@example.org"]);
        let link = next(&h).expect("next");
        assert!(link.contains("prefix=b&"), "{link}");
        let (_, h, b) = env.call("GET", &link, Some("org"), None).await;
        assert_eq!(
            (logins(&b), next(&h)),
            (vec!["bob@example.org".to_owned()], None)
        );
        let (_, _, b) = env
            .call("GET", &format!("{base}?prefix=nobody"), Some("org"), None)
            .await;
        assert_eq!(b, json!([]));

        // Организации не смешиваются.
        let other = format!("/api/v1/orgs/{}/accounts", env.other_tenant);
        let (_, _, b) = env.call("GET", &other, Some("other"), None).await;
        assert_eq!(logins(&b), ["z@other.test"]);

        // Поиск по `login` — как прежде; вместе с параметрами списка — 400.
        let (_, h, b) = env
            .call(
                "GET",
                &format!("{base}?login=C@EXAMPLE.ORG"),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(
            (logins(&b), next(&h)),
            (vec!["c@example.org".to_owned()], None)
        );
        for q in [
            "login=c@example.org&limit=1",
            "login=c@example.org&prefix=c",
            "login=a&after=a",
        ] {
            let (s, _, _) = env
                .call("GET", &format!("{base}?{q}"), Some("org"), None)
                .await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{q}");
        }
        let (s, _, _) = env
            .call("GET", &format!("{base}?limit=x"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    /// Строка запроса из `AccountQuery` тем же обходом, что у клиента: поля JSON — пары «имя=значение»,
    /// `null` пропускается, значения — в `%XX`.
    fn query_string(q: &AccountQuery) -> String {
        let Value::Object(fields) = serde_json::to_value(q).expect("json") else {
            panic!("AccountQuery — не объект");
        };
        fields
            .into_iter()
            .filter_map(|(k, v)| match v {
                Value::Null => None,
                Value::String(s) => Some(format!("{k}={}", encode_query(&s))),
                other => Some(format!("{k}={other}")),
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Разбор строки запроса тем же извлекателем, что у обработчика списка.
    fn parse_query(query: &str) -> Option<AccountQuery> {
        let uri: axum::http::Uri = format!("/accounts?{query}").parse().expect("uri");
        axum::extract::Query::<AccountQuery>::try_from_uri(&uri)
            .ok()
            .map(|q| q.0)
    }

    /// ADM-23: `AccountQuery` обходит строку запроса без потерь — кириллица, `+`, `&`, `=`, пробел, `%`.
    #[test]
    fn account_query_from_query_string() {
        let search = AccountQuery {
            login: Some("Юля+тест@пример.рф".into()),
            ..AccountQuery::default()
        };
        let list = AccountQuery {
            login: None,
            prefix: Some("ю+ a&b=c%".into()),
            after: Some("юля+тест@пример.рф".into()),
            limit: Some(25),
        };
        for q in [search.clone(), list, AccountQuery::default()] {
            let s = query_string(&q);
            assert!(s.is_ascii(), "{s}");
            assert_eq!(parse_query(&s), Some(q), "{s}");
        }
        assert_eq!(
            query_string(&search),
            "login=%D0%AE%D0%BB%D1%8F%2B%D1%82%D0%B5%D1%81%D1%82@%D0%BF%D1%80%D0%B8%D0%BC%D0%B5%D1%80.%D1%80%D1%84"
        );

        // Параметры по одному и вместе; пустая строка — все поля пусты.
        assert_eq!(
            parse_query("prefix=a&after=a%40b.test&limit=3"),
            Some(AccountQuery {
                login: None,
                prefix: Some("a".into()),
                after: Some("a@b.test".into()),
                limit: Some(3),
            })
        );
        assert_eq!(parse_query(""), Some(AccountQuery::default()));
        // Незакодированный `+` в строке запроса — пробел (application/x-www-form-urlencoded): плюс — только `%2B`.
        assert_eq!(
            parse_query("login=a+b@x.test").and_then(|q| q.login),
            Some("a b@x.test".to_owned())
        );
        // Неизвестный и повторный параметр, не число и отрицательное число в `limit` — отказ.
        for bad in [
            "page=2",
            "limit=x",
            "limit=-1",
            "limit=1&limit=2",
            "prefix=a&offset=1",
        ] {
            assert_eq!(parse_query(bad), None, "{bad}");
        }
    }

    /// ADM-23: строка запроса через роутер — поиск и список с кириллицей и `+`, отказы — 400.
    #[tokio::test]
    async fn account_query_over_router() {
        let env = Env::new();
        for l in [
            "Юля+тест@пример.рф",
            "юра@пример.рф",
            "юля@пример.рф",
            "a+b@example.org",
            "a b@example.org",
        ] {
            env.account(l).await;
        }
        let base = format!("{}/accounts", org());
        let get = async |q: &AccountQuery| {
            let path = format!("{base}?{}", query_string(q));
            env.call("GET", &path, Some("org"), None).await
        };

        // Поиск: кириллица и `+` доходят до хранилища без потерь (`+` не превращается в пробел). Регистр кириллицы
        // здесь тот же: хранилище в памяти сравнивает имена без учёта регистра только для ASCII.
        let (s, _, b) = get(&AccountQuery {
            login: Some("Юля+тест@пример.рф".into()),
            ..AccountQuery::default()
        })
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(logins(&b), ["Юля+тест@пример.рф"]);
        let (_, _, b) = get(&AccountQuery {
            login: Some("a+b@example.org".into()),
            ..AccountQuery::default()
        })
        .await;
        assert_eq!(logins(&b), ["a+b@example.org"]);
        // Незакодированный `+` — пробел: находится запись с пробелом, а не с плюсом.
        let raw = format!("{base}?login=a+b@example.org");
        let (_, _, b) = env.call("GET", &raw, Some("org"), None).await;
        assert_eq!(logins(&b), ["a b@example.org"]);

        // Список: prefix, limit и after вместе; ссылка продолжения читается тем же разбором.
        let (s, h, b) = get(&AccountQuery {
            prefix: Some("Ю".into()),
            limit: Some(1),
            ..AccountQuery::default()
        })
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(logins(&b), ["Юля+тест@пример.рф"]);
        let link = next(&h).expect("next");
        assert!(link.is_ascii(), "{link}");
        let (_, query) = link.split_once('?').expect("query");
        assert_eq!(
            parse_query(query),
            Some(AccountQuery {
                login: None,
                prefix: Some("ю".into()),
                after: Some("юля+тест@пример.рф".into()),
                limit: Some(1),
            })
        );
        let (_, h, b) = env.call("GET", &link, Some("org"), None).await;
        assert_eq!(logins(&b), ["юля@пример.рф"]);
        let (_, h, b) = env
            .call("GET", &next(&h).expect("next"), Some("org"), None)
            .await;
        assert_eq!(logins(&b), ["юра@пример.рф"]);
        assert!(next(&h).is_none(), "страница последняя");
        let (_, h, b) = get(&AccountQuery {
            prefix: Some("юля+".into()),
            after: Some("юля+".into()),
            limit: Some(5),
            ..AccountQuery::default()
        })
        .await;
        assert_eq!(logins(&b), ["Юля+тест@пример.рф"]);
        assert!(next(&h).is_none());

        // `login` с любым параметром списка — 400 с пояснением (RFC 9457).
        for rest in [
            AccountQuery {
                prefix: Some("ю".into()),
                ..AccountQuery::default()
            },
            AccountQuery {
                after: Some("юля@пример.рф".into()),
                ..AccountQuery::default()
            },
            AccountQuery {
                limit: Some(1),
                ..AccountQuery::default()
            },
        ] {
            let q = AccountQuery {
                login: Some("юля@пример.рф".into()),
                ..rest
            };
            let (s, _, b) = get(&q).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{q:?}");
            assert_eq!(b["status"], 400, "{b}");
            assert!(
                b["detail"].as_str().is_some_and(|d| d.contains("login")),
                "{b}"
            );
        }
        // Неизвестный параметр и не число в `limit` — 400.
        for bad in ["page=2", "limit=x", "login=a&offset=1"] {
            let path = format!("{base}?{bad}");
            let (s, _, _) = env.call("GET", &path, Some("org"), None).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    /// Размер страницы: по умолчанию — `page_size`, больший `max_page_size` урезается, 0 — одна запись.
    #[tokio::test]
    async fn account_page_limit() {
        let cfg = AdminConfig {
            page_size: 2,
            max_page_size: 3,
            ..config()
        };
        let env = Env::with(cfg, Vec::new());
        for l in ["a@e.test", "b@e.test", "c@e.test", "d@e.test", "e@e.test"] {
            env.account(l).await;
        }
        let base = format!("{}/accounts", org());
        let (_, h, b) = env.call("GET", &base, Some("org"), None).await;
        assert_eq!(logins(&b), ["a@e.test", "b@e.test"]);
        assert!(next(&h).expect("next").contains("limit=2&"));
        let (_, h, b) = env
            .call("GET", &format!("{base}?limit=1000"), Some("org"), None)
            .await;
        assert_eq!(logins(&b), ["a@e.test", "b@e.test", "c@e.test"]);
        let link = next(&h).expect("next");
        assert!(link.contains("limit=3&after=c@e.test"), "{link}");
        let (_, h, b) = env.call("GET", &link, Some("org"), None).await;
        assert_eq!(
            (logins(&b), next(&h)),
            (vec!["d@e.test".to_owned(), "e@e.test".to_owned()], None)
        );
        let (_, _, b) = env
            .call("GET", &format!("{base}?limit=0"), Some("org"), None)
            .await;
        assert_eq!(logins(&b), ["a@e.test"]);
    }

    /// Права: администраторы и оператор своей организации — да; аудитор, без роли, чужая организация — 403.
    #[tokio::test]
    async fn account_list_roles() {
        let env = Env::new();
        env.account("a@example.org").await;
        let base = format!("{}/accounts", org());
        for (token, status) in [
            ("root", StatusCode::OK),
            ("org", StatusCode::OK),
            ("operator", StatusCode::OK),
            ("auditor", StatusCode::FORBIDDEN),
            ("user", StatusCode::FORBIDDEN),
            ("other", StatusCode::FORBIDDEN),
        ] {
            let (s, _, _) = env.call("GET", &base, Some(token), None).await;
            assert_eq!(s, status, "{token}");
        }
        let (s, _, _) = env.call("GET", &base, None, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let other = format!("/api/v1/orgs/{}/accounts", env.other_tenant);
        let (s, _, _) = env.call("GET", &other, Some("org"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env.call("GET", &other, Some("root"), None).await;
        assert_eq!(s, StatusCode::OK);
    }

    /// Приёмка ADM-18: блокировка и имя локальной записи, запись каталога — 409, нет записи — 404.
    #[tokio::test]
    async fn patch_account() {
        use tp_contracts::meta::{AccountStore, AccountUpsert};

        let env = Env::new();
        let scope = env.account("user@example.org").await;
        let one = format!("{}/accounts/{}", org(), scope.account);
        let (s, _, b) = env
            .call("PATCH", &one, Some("org"), Some(json!({"enabled": false})))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["enabled"], false);
        assert_eq!(b["login"], "user@example.org");
        let (_, _, b) = env.call("GET", &one, Some("org"), None).await;
        assert_eq!(b["enabled"], false);
        let last = env.audit.records().pop().expect("audit");
        assert_eq!(
            (last.method.as_str(), last.route.as_str(), last.status),
            ("PATCH", "/api/v1/orgs/{org}/accounts/{account}", 200)
        );
        assert_eq!(last.path, one);

        // Имя меняется отдельно, блокировка сохраняется; разблокировка.
        let body = json!({"display_name": " Анна Петрова "});
        let (s, _, b) = env.call("PATCH", &one, Some("org"), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            (&b["display_name"], &b["enabled"]),
            (&json!("Анна Петрова"), &json!(false))
        );
        let body = json!({"enabled": true, "display_name": "Анна"});
        let (_, _, b) = env.call("PATCH", &one, Some("org"), Some(body)).await;
        assert_eq!(
            (&b["display_name"], &b["enabled"]),
            (&json!("Анна"), &json!(true))
        );
        assert_eq!(b["id"], scope.account.to_string(), "запись та же");

        // Пустое тело и неизвестное поле — 422; пустое имя — 400.
        for bad in [json!({}), json!({"login": "x@example.org"})] {
            let (s, _, _) = env.call("PATCH", &one, Some("org"), Some(bad)).await;
            assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        }
        for bad in [
            json!({"display_name": "  "}),
            json!({"display_name": "a\nb"}),
        ] {
            let (s, _, _) = env.call("PATCH", &one, Some("org"), Some(bad)).await;
            assert_eq!(s, StatusCode::BAD_REQUEST);
        }

        // Запись каталога: имя и блокировку ведёт каталог.
        let dir = env
            .meta
            .upsert(
                TenantId::DEFAULT,
                AccountUpsert {
                    domain: tp_contracts::DomainId::new(),
                    login: "dir@example.org".into(),
                    display_name: "Из каталога".into(),
                    external_id: Some("guid-1".into()),
                    enabled: true,
                },
            )
            .await
            .expect("upsert");
        let dir_url = format!("{}/accounts/{}", org(), dir.id);
        for body in [
            json!({"display_name": "Другое"}),
            json!({"enabled": false}),
            json!({"display_name": "Другое", "enabled": false}),
        ] {
            let (s, _, b) = env.call("PATCH", &dir_url, Some("org"), Some(body)).await;
            assert_eq!(s, StatusCode::CONFLICT, "{b}");
            assert!(b["detail"].as_str().expect("detail").contains("каталог"));
        }
        let (_, _, b) = env.call("GET", &dir_url, Some("org"), None).await;
        assert_eq!(
            (&b["display_name"], &b["enabled"]),
            (&json!("Из каталога"), &json!(true))
        );

        // Нет записи — 404; оператор и чужая организация — 403.
        let missing = format!("{}/accounts/{}", org(), tp_contracts::AccountId::new());
        let block = || Some(json!({"enabled": false}));
        let (s, _, _) = env.call("PATCH", &missing, Some("org"), block()).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        for token in ["operator", "user", "other"] {
            let (s, _, _) = env.call("PATCH", &one, Some(token), block()).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{token}");
        }
        let (_, _, spec) = env.call("GET", "/api/openapi.json", None, None).await;
        let op = &spec["paths"]["/api/v1/orgs/{org}/accounts/{account}"]["patch"];
        assert_eq!(op["operationId"], "patch_account");
    }

    #[test]
    fn query_encoding() {
        assert_eq!(encode_query("a+b@x.test"), "a%2Bb@x.test");
        assert_eq!(encode_query("a&b=c d"), "a%26b%3Dc%20d");
        assert_eq!(encode_query("я"), "%D1%8F");
    }

    #[tokio::test]
    async fn openapi_lists_routes() {
        let env = Env::new();
        let (_, _, b) = env.call("GET", "/api/openapi.json", None, None).await;
        let paths = [
            "/api/v1/orgs/{org}",
            "/api/v1/orgs/{org}/transport/domains/{id}/policy",
            "/api/v1/orgs/{org}/accounts/{account}",
        ];
        for p in paths {
            assert!(b["paths"][p].is_object(), "{p}");
        }
        // Домены организации — один ресурс `transport/domains` (решение 94): прежних операций в схеме нет.
        let paths = b["paths"].as_object().expect("paths");
        assert!(
            !paths
                .keys()
                .any(|p| p.starts_with("/api/v1/orgs/{org}/domains")),
            "{:?}",
            paths.keys().collect::<Vec<_>>()
        );
        for name in ["DomainInfo", "DomainDetails", "NewDomain"] {
            assert!(b["components"]["schemas"][name].is_null(), "{name}");
        }
        assert!(b["components"]["schemas"]["PolicyBody"].is_object());
        let list = &b["paths"]["/api/v1/orgs/{org}/accounts"]["get"];
        assert!(
            list["responses"]["200"]["headers"]["Link"].is_object(),
            "{list}"
        );
        let params: Vec<_> = list["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .map(|p| p["name"].as_str().expect("name"))
            .collect();
        for p in ["login", "prefix", "after", "limit"] {
            assert!(params.contains(&p), "{p}: {params:?}");
        }
    }
}
