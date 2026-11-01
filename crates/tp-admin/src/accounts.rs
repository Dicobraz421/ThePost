//! Организация, домены, учётные записи и квоты (SRS 5.2, 3.8).
//!
//! - организация (`/api/v1/orgs/{org}`): чтение и политика уровня организации;
//! - домены: список, создание, удаление, политика домена;
//! - учётные записи приходят из каталога (LDAP): здесь — поиск по имени входа, чтение с использованием квоты и
//!   только локальные атрибуты — политика ящика (квота, срок корзины, лимиты отправки);
//! - политика уровня задаётся целиком (`PUT`), поле `null` — не задано на уровне; действующая политика
//!   (`effective`) — наложение цепочки организация → домен → ящик (`PolicyResolver`).
//!
//! Записи политик и доменов в контрактах пока нет: [`PolicyStore`] и [`DomainStore`] — временные трейты полосы
//! (как `AppPasswordStore` в AUTH-02 до CTR-22). `PolicyStore` повторяет `PgMeta::policy`/`set_policy`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tp_contracts::config::duration;
use tp_contracts::meta::{Account, Policy};
use tp_contracts::{AccountId, DomainId, Error, Result, TenantId};
use utoipa::{OpenApi, ToSchema};

use crate::server::{Admin, ApiResult, Problem};

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

/// Почтовый домен организации.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domain {
    /// Идентификатор.
    pub id: DomainId,
    /// Имя (нижний регистр, A-label).
    pub name: String,
}

/// Домены организаций (временный трейт до контракта).
#[async_trait]
pub trait DomainStore: Send + Sync + 'static {
    /// Домены организации по имени.
    async fn list(&self, tenant: TenantId) -> Result<Vec<Domain>>;

    /// Создаёт домен; имя занято в инсталляции — `Conflict`.
    async fn create(&self, tenant: TenantId, name: &str) -> Result<Domain>;

    /// Удаляет домен; нет — `NotFound`, у домена есть учётные записи — `Conflict`.
    async fn delete(&self, tenant: TenantId, id: DomainId) -> Result<()>;
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
    /// Лимит получателей одного письма.
    #[serde(default)]
    pub max_recipients: Option<u32>,
    /// Селектор ключа DKIM.
    #[serde(default)]
    pub dkim_selector: Option<String>,
    /// Профиль Milter.
    #[serde(default)]
    pub milter_profile: Option<String>,
}

impl From<Policy> for PolicyBody {
    fn from(p: Policy) -> Self {
        Self {
            quota_bytes: p.quota_bytes,
            trash_retention: p.trash_retention,
            retention: p.retention,
            send_per_hour: p.send_per_hour,
            max_recipients: p.max_recipients,
            dkim_selector: p.dkim_selector,
            milter_profile: p.milter_profile,
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
            max_recipients: p.max_recipients,
            dkim_selector: p.dkim_selector,
            milter_profile: p.milter_profile,
            // `trash.purge` в Admin API — ADM-09.
            trash_purge: None,
        }
    }
}

/// Политика уровня и действующая политика.
#[derive(Debug, Serialize, ToSchema)]
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

/// Домен.
#[derive(Debug, Serialize, ToSchema)]
pub struct DomainInfo {
    /// Идентификатор.
    pub id: String,
    /// Имя.
    pub name: String,
}

/// Домен с политиками.
#[derive(Debug, Serialize, ToSchema)]
pub struct DomainDetails {
    /// Домен.
    #[serde(flatten)]
    pub domain: DomainInfo,
    /// Политики.
    #[serde(flatten)]
    pub policies: Policies,
}

/// Новый домен.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewDomain {
    /// Имя (A-label; приводится к нижнему регистру).
    pub name: String,
}

/// Использование квоты.
#[derive(Debug, Serialize, ToSchema)]
pub struct QuotaInfo {
    /// Занято байт, включая корзину.
    pub used_bytes: u64,
    /// Число писем.
    pub messages: u64,
    /// Лимит; `null` — без ограничения.
    pub limit_bytes: Option<u64>,
}

/// Учётная запись.
#[derive(Debug, Serialize, ToSchema)]
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

/// Поиск учётной записи.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub struct AccountQuery {
    /// Имя входа (без учёта регистра).
    pub login: String,
}

/// Маршруты организации, доменов и учётных записей.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route("/api/v1/orgs/{org}", get(get_org))
        .route("/api/v1/orgs/{org}/policy", put(put_org_policy))
        .route(
            "/api/v1/orgs/{org}/domains",
            get(list_domains).post(create_domain),
        )
        .route(
            "/api/v1/orgs/{org}/domains/{domain}",
            get(get_domain).delete(delete_domain),
        )
        .route(
            "/api/v1/orgs/{org}/domains/{domain}/policy",
            put(put_domain_policy),
        )
        .route("/api/v1/orgs/{org}/accounts", get(find_account))
        .route("/api/v1/orgs/{org}/accounts/{account}", get(get_account))
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
        list_domains,
        create_domain,
        get_domain,
        delete_domain,
        put_domain_policy,
        find_account,
        get_account,
        put_account_policy
    ),
    components(schemas(
        PolicyBody,
        Policies,
        Organization,
        DomainInfo,
        DomainDetails,
        NewDomain,
        QuotaInfo,
        AccountInfo
    ))
)]
pub(crate) struct Doc;

/// Учётная запись организации; нет — `NotFound`.
pub(crate) async fn account(a: &Admin, org: TenantId, id: AccountId) -> Result<Account> {
    a.d.accounts.get(org, id).await?.ok_or(Error::NotFound)
}

async fn domain(a: &Admin, org: TenantId, id: DomainId) -> Result<Domain> {
    a.d.domains
        .list(org)
        .await?
        .into_iter()
        .find(|d| d.id == id)
        .ok_or(Error::NotFound)
}

async fn policies(
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

/// Имя домена: A-label, 2–127 меток `[a-z0-9-]` до 63 символов без `-` по краям, всего до 253.
fn domain_name(name: &str) -> Result<String> {
    let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
    let label_ok = |l: &str| {
        (1..=63).contains(&l.len())
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if name.len() > 253 || name.split('.').count() < 2 || !name.split('.').all(label_ok) {
        return Err(Error::invalid(format!(
            "недопустимое имя домена {name:?} (нужен A-label, например xn--80a.example)"
        )));
    }
    Ok(name)
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

/// Домены организации.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/domains", tag = "domains",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = Vec<DomainInfo>), (status = 403, body = Problem)))]
async fn list_domains(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<Vec<DomainInfo>>> {
    let list = a.d.domains.list(org).await?;
    Ok(Json(list.into_iter().map(DomainInfo::from).collect()))
}

impl From<Domain> for DomainInfo {
    fn from(d: Domain) -> Self {
        Self {
            id: d.id.to_string(),
            name: d.name,
        }
    }
}

/// Создаёт домен.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/domains", tag = "domains",
    params(("org" = String, Path, description = "Организация (UUID)")),
    request_body = NewDomain,
    responses((status = 201, body = DomainInfo), (status = 400, body = Problem), (status = 409, body = Problem)))]
async fn create_domain(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Json(body): Json<NewDomain>,
) -> ApiResult<(StatusCode, Json<DomainInfo>)> {
    let name = domain_name(&body.name)?;
    let d = a.d.domains.create(org, &name).await?;
    Ok((StatusCode::CREATED, Json(d.into())))
}

/// Домен и его политика.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/domains/{domain}", tag = "domains",
    params(("org" = String, Path, description = "Организация (UUID)"), ("domain" = String, Path, description = "Домен (UUID)")),
    responses((status = 200, body = DomainDetails), (status = 404, body = Problem)))]
async fn get_domain(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, DomainId)>,
) -> ApiResult<Json<DomainDetails>> {
    let d = domain(&a, org, id).await?;
    Ok(Json(DomainDetails {
        domain: d.into(),
        policies: policies(&a, org, PolicyTarget::Domain(id), Some(id), None).await?,
    }))
}

/// Удаляет домен без учётных записей.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/domains/{domain}", tag = "domains",
    params(("org" = String, Path, description = "Организация (UUID)"), ("domain" = String, Path, description = "Домен (UUID)")),
    responses((status = 204), (status = 404, body = Problem), (status = 409, body = Problem)))]
async fn delete_domain(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, DomainId)>,
) -> ApiResult<StatusCode> {
    a.d.domains.delete(org, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Задаёт политику домена целиком.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/domains/{domain}/policy", tag = "domains",
    params(("org" = String, Path, description = "Организация (UUID)"), ("domain" = String, Path, description = "Домен (UUID)")),
    request_body = PolicyBody,
    responses((status = 200, body = PolicyBody), (status = 404, body = Problem)))]
async fn put_domain_policy(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, DomainId)>,
    Json(body): Json<PolicyBody>,
) -> ApiResult<Json<PolicyBody>> {
    domain(&a, org, id).await?;
    a.d.policies
        .set_policy(org, PolicyTarget::Domain(id), &body.clone().into())
        .await?;
    Ok(Json(body))
}

/// Учётная запись по имени входа: список из одной записи или пустой.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts", tag = "accounts",
    params(("org" = String, Path, description = "Организация (UUID)"), AccountQuery),
    responses((status = 200, body = Vec<AccountInfo>), (status = 403, body = Problem)))]
async fn find_account(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Query(q): Query<AccountQuery>,
) -> ApiResult<Json<Vec<AccountInfo>>> {
    let mut out = Vec::new();
    if let Some(acc) = a.d.accounts.by_login(org, &q.login).await? {
        out.push(account_info(&a, acc).await?);
    }
    Ok(Json(out))
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

    use super::domain_name;
    use crate::testenv::Env;

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

    #[tokio::test]
    async fn domains_crud() {
        let env = Env::new();
        let base = format!("{}/domains", org());
        let new = |name: &str| Some(json!({ "name": name }));
        let (s, _, d) = env
            .call("POST", &base, Some("org"), new("Example.ORG."))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{d}");
        assert_eq!(d["name"], "example.org");
        let id = d["id"].as_str().expect("id").to_owned();
        let (s, _, _) = env
            .call("POST", &base, Some("root"), new("example.org"))
            .await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _, b) = env.call("POST", &base, Some("org"), new("пример.рф")).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(b["detail"].as_str().expect("detail").contains("A-label"));
        env.call("POST", &base, Some("org"), new("a.example")).await;
        let (_, _, list) = env.call("GET", &base, Some("org"), None).await;
        let names: Vec<_> = list
            .as_array()
            .expect("list")
            .iter()
            .map(|d| d["name"].clone())
            .collect();
        assert_eq!(names, vec![json!("a.example"), json!("example.org")]);

        let one = format!("{base}/{id}");
        let dpolicy = format!("{one}/policy");
        let (s, _, _) = env
            .call(
                "PUT",
                &dpolicy,
                Some("org"),
                Some(json!({"max_recipients": 50})),
            )
            .await;
        assert_eq!(s, StatusCode::OK);
        let opolicy = json!({"max_recipients": 10, "send_per_hour": 100});
        env.call(
            "PUT",
            &format!("{}/policy", org()),
            Some("org"),
            Some(opolicy),
        )
        .await;
        let (s, _, b) = env.call("GET", &one, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["name"], "example.org");
        assert_eq!(b["policy"]["max_recipients"], 50);
        assert_eq!(b["policy"]["send_per_hour"], Value::Null);
        assert_eq!(b["effective"]["max_recipients"], 50);
        assert_eq!(b["effective"]["send_per_hour"], 100);

        let (s, _, _) = env.call("DELETE", &one, Some("org"), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _, _) = env.call("DELETE", &one, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = env.call("GET", &one, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = env
            .call("PUT", &dpolicy, Some("org"), Some(json!({})))
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
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

        let opolicy = json!({"quota_bytes": 100, "send_per_hour": 7});
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

    #[test]
    fn domain_names() {
        let ok = |n: &str| domain_name(n).expect(n);
        assert_eq!(ok(" Mail.Example.ORG "), "mail.example.org");
        assert_eq!(ok("xn--80a1acny.xn--p1ai"), "xn--80a1acny.xn--p1ai");
        let long = format!("{}.example", "a".repeat(64));
        let bad = [
            "",
            "localhost",
            "-a.example",
            "a-.example",
            "a..example",
            "a_b.example",
        ];
        for n in bad.iter().copied().chain(["пример.рф", long.as_str()]) {
            assert!(domain_name(n).is_err(), "{n:?}");
        }
    }

    #[tokio::test]
    async fn openapi_lists_routes() {
        let env = Env::new();
        let (_, _, b) = env.call("GET", "/api/openapi.json", None, None).await;
        let paths = [
            "/api/v1/orgs/{org}",
            "/api/v1/orgs/{org}/domains/{domain}/policy",
            "/api/v1/orgs/{org}/accounts/{account}",
        ];
        for p in paths {
            assert!(b["paths"][p].is_object(), "{p}");
        }
        assert!(b["components"]["schemas"]["PolicyBody"].is_object());
    }
}
