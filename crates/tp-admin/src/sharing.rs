//! Общий доступ к календарям и адресным книгам, представители (SRS 4.4, 4.5, 5.2; APP-I; ADM-16; контракты CTR-48):
//!
//! - политика общего доступа организации (`/api/v1/orgs/{org}/calendar-sharing`): кому можно выдавать права на свои
//!   календари и адресные книги и наибольшие права вне групп политики и «всем» (`anyone`); не задана — по умолчанию
//!   (`SharingPolicy::default`);
//! - представители учётной записи (`…/accounts/{account}/delegates`): список, назначение или замена
//!   (`PUT …/delegates/{delegate}`), отзыв (`DELETE`); представитель читает или ведёт все календари владельца и
//!   может планировать встречи от его имени;
//! - календари и адресные книги учётной записи (`…/accounts/{account}/collections`, без книги GAL организации) и
//!   права на каждую (`…/collections/{collection}/acl`): записи по
//!   субъектам — имя входа, имя группы, `anyone`; `PUT …/acl/{identifier}` заменяет права субъекта целиком, пустые
//!   права отзывают запись.
//!
//! Выдача прав и назначение представителя — в пределах политики организации: проверяет единый слой прав
//! (`SharingAuthorizer::check_grant`, `check_delegate` в `tp_auth::CollectionAuthorizer`) от имени владельца —
//! администратор действует за него; сверх политики — 409 с пояснением. Ужесточение политики уже выданное не удаляет: его ограничивает слой авторизации
//! (`SharingPolicy::limit`, `tp-auth`). Ресурсы и правила бронирования — [`crate::resources`]. Хранилища подключает
//! [`Admin::with_sharing`]; без них маршруты отвечают 503. Изменения пишет в журнал аудита слой доступа
//! ([`crate::server`]).

use std::fmt;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, put};
use serde::{Deserialize, Serialize};
use tp_auth::CollectionAuthorizer;
use tp_contracts::auth::Principal;
use tp_contracts::dav::{CollectionId, CollectionKind, DavStore};
use tp_contracts::meta::{Account, AccountScope};
use tp_contracts::sharing::{
    ANYONE, CollectionAcl, CollectionAclEntry, CollectionRights, Delegate, DelegateLevel,
    DelegateStore, ResourceStore, SharingAuthorizer, SharingPolicy, SharingPolicyStore,
    SharingScope, validate_identifier,
};
use tp_contracts::{AccountId, Error, GroupId, Result, TenantId};
use utoipa::{OpenApi, ToSchema};

use crate::accounts::account;
use crate::extract::{Json, Path};
use crate::server::{Admin, ApiResult, Problem, Refused};

/// Хранилища общего доступа, представителей и ресурсов (CTR-48; реализации — `tp-meta`).
#[derive(Clone)]
pub struct SharingStores {
    /// Права на коллекции.
    pub acl: Arc<dyn CollectionAcl>,
    /// Представители учётных записей.
    pub delegates: Arc<dyn DelegateStore>,
    /// Правила бронирования ресурсов.
    pub resources: Arc<dyn ResourceStore>,
    /// Политика общего доступа организаций.
    pub policy: Arc<dyn SharingPolicyStore>,
    /// Календари и адресные книги учётных записей: список коллекций для выдачи прав.
    pub collections: Arc<dyn DavStore>,
}

impl fmt::Debug for SharingStores {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharingStores").finish_non_exhaustive()
    }
}

impl Admin {
    /// Подключает общий доступ, представителей и ресурсы ([`crate::sharing`], [`crate::resources`]).
    pub fn with_sharing(mut self, stores: SharingStores) -> Self {
        self.sharing = Some(stores);
        self
    }
}

/// Хранилища общего доступа; не подключены — `Unavailable`.
pub(crate) fn stores(a: &Admin) -> Result<&SharingStores> {
    a.sharing.as_ref().ok_or(Error::UNAVAILABLE)
}

/// Права на коллекцию: старшее включает младшие (`admin` — все, `write` — `read`, `read` — `free_busy`);
/// `schedule` — отдельное. Отсутствующее поле — `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RightsBody {
    /// Видеть занятость без подробностей.
    pub free_busy: bool,
    /// Читать объекты.
    pub read: bool,
    /// Создавать, изменять и удалять объекты.
    pub write: bool,
    /// Планировать от имени владельца: приглашения и ответы.
    pub schedule: bool,
    /// Менять свойства коллекции, удалять её и выдавать права.
    pub admin: bool,
}

impl From<CollectionRights> for RightsBody {
    fn from(r: CollectionRights) -> Self {
        Self {
            free_busy: r.free_busy,
            read: r.read,
            write: r.write,
            schedule: r.schedule,
            admin: r.admin,
        }
    }
}

impl From<RightsBody> for CollectionRights {
    fn from(r: RightsBody) -> Self {
        Self {
            free_busy: r.free_busy,
            read: r.read,
            write: r.write,
            schedule: r.schedule,
            admin: r.admin,
        }
    }
}

/// Запись прав на коллекцию.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AclEntryBody {
    /// Субъект: имя входа, имя группы или `anyone`.
    pub identifier: String,
    /// Права, как выданы (без включённых младших).
    pub rights: RightsBody,
}

impl From<CollectionAclEntry> for AclEntryBody {
    fn from(e: CollectionAclEntry) -> Self {
        Self {
            identifier: e.identifier,
            rights: e.rights.into(),
        }
    }
}

/// Кому в организации можно выдавать права на свои календари и адресные книги.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
pub enum SharingScopeBody {
    /// Общий доступ запрещён: права и представители не выдаются.
    Nobody,
    /// Полные права — участникам перечисленных групп (с вложенными), остальным субъектам — не больше
    /// `max_outside`.
    Groups {
        /// Группы (UUID).
        #[schema(value_type = Vec<String>)]
        groups: Vec<GroupId>,
    },
    /// Любому субъекту организации — полные права.
    #[default]
    Organization,
}

/// Политика общего доступа организации к календарям и адресным книгам.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SharingPolicyBody {
    /// Кому можно выдавать права.
    pub scope: SharingScopeBody,
    /// Наибольшие права субъекту вне групп политики и субъекту `anyone`.
    pub max_outside: RightsBody,
}

impl Default for SharingPolicyBody {
    fn default() -> Self {
        SharingPolicy::default().into()
    }
}

impl From<SharingPolicy> for SharingPolicyBody {
    fn from(p: SharingPolicy) -> Self {
        Self {
            scope: match p.scope {
                SharingScope::Nobody => SharingScopeBody::Nobody,
                SharingScope::Groups { groups } => SharingScopeBody::Groups { groups },
                SharingScope::Organization => SharingScopeBody::Organization,
            },
            max_outside: p.max_outside.into(),
        }
    }
}

impl From<SharingPolicyBody> for SharingPolicy {
    fn from(p: SharingPolicyBody) -> Self {
        Self {
            scope: match p.scope {
                SharingScopeBody::Nobody => SharingScope::Nobody,
                SharingScopeBody::Groups { groups } => SharingScope::Groups { groups },
                SharingScopeBody::Organization => SharingScope::Organization,
            },
            max_outside: p.max_outside.into(),
        }
    }
}

/// Уровень доступа представителя ко всем календарям владельца.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DelegateLevelBody {
    /// Чтение.
    Read,
    /// Чтение и запись.
    Write,
}

impl From<DelegateLevel> for DelegateLevelBody {
    fn from(l: DelegateLevel) -> Self {
        match l {
            DelegateLevel::Read => Self::Read,
            DelegateLevel::Write => Self::Write,
        }
    }
}

impl From<DelegateLevelBody> for DelegateLevel {
    fn from(l: DelegateLevelBody) -> Self {
        match l {
            DelegateLevelBody::Read => Self::Read,
            DelegateLevelBody::Write => Self::Write,
        }
    }
}

/// Назначение представителя.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DelegateBody {
    /// Уровень доступа к календарям.
    pub level: DelegateLevelBody,
    /// Отправляет приглашения и ответы от имени владельца.
    #[serde(default)]
    pub schedule_on_behalf: bool,
}

/// Представитель учётной записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DelegateInfo {
    /// Учётная запись представителя (UUID).
    pub delegate: String,
    /// Имя входа представителя.
    pub login: String,
    /// Отображаемое имя представителя.
    pub display_name: String,
    /// Уровень доступа к календарям.
    pub level: DelegateLevelBody,
    /// Отправляет приглашения и ответы от имени владельца.
    pub schedule_on_behalf: bool,
}

const POLICY: &str = "/api/v1/orgs/{org}/calendar-sharing";
const DELEGATES: &str = "/api/v1/orgs/{org}/accounts/{account}/delegates";
const DELEGATE: &str = "/api/v1/orgs/{org}/accounts/{account}/delegates/{delegate}";
const COLLECTIONS: &str = "/api/v1/orgs/{org}/accounts/{account}/collections";
const ACL: &str = "/api/v1/orgs/{org}/accounts/{account}/collections/{collection}/acl";
const ACL_ENTRY: &str =
    "/api/v1/orgs/{org}/accounts/{account}/collections/{collection}/acl/{identifier}";

/// Маршруты общего доступа и представителей.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route(POLICY, get(get_policy).put(put_policy))
        .route(DELEGATES, get(list_delegates))
        .route(DELEGATE, put(put_delegate).delete(remove_delegate))
        .route(COLLECTIONS, get(list_collections))
        .route(ACL, get(get_acl))
        .route(ACL_ENTRY, put(put_acl))
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(
        get_policy,
        put_policy,
        list_delegates,
        put_delegate,
        remove_delegate,
        list_collections,
        get_acl,
        put_acl
    ),
    components(schemas(
        RightsBody,
        AclEntryBody,
        SharingScopeBody,
        SharingPolicyBody,
        DelegateLevelBody,
        DelegateBody,
        DelegateInfo,
        CollectionKindBody,
        CollectionInfo
    ))
)]
pub(crate) struct Doc;

/// Отказ политики общего доступа: 409 с пояснением.
fn beyond_policy(subject: &str) -> Refused {
    Refused::Problem(
        StatusCode::CONFLICT,
        format!(
            "политика общего доступа организации (calendar-sharing) не допускает такие права для {subject}"
        ),
    )
}

/// Единый слой прав на коллекции (`tp-auth`) поверх хранилищ общего доступа и групп, учётных записей Admin API.
fn authorizer(a: &Admin, s: &SharingStores) -> CollectionAuthorizer {
    CollectionAuthorizer::new(tp_auth::SharingStores {
        acl: s.acl.clone(),
        delegates: s.delegates.clone(),
        policy: s.policy.clone(),
        dav: s.collections.clone(),
        groups: a.d.groups.clone(),
        accounts: a.d.accounts.clone(),
    })
}

/// Владелец как субъект проверки: администратор выдаёт права и назначает представителей от его имени.
fn as_owner(acc: &Account) -> Principal {
    Principal {
        tenant: acc.tenant,
        account: acc.id,
        login: acc.login.clone(),
        groups: Default::default(),
        scopes: None,
    }
}

/// Отказ слоя прав: сверх политики (`Forbidden`) — 409 с пояснением, остальное — как есть.
fn refused(e: Error, subject: &str) -> Refused {
    match e {
        Error::Forbidden => beyond_policy(subject),
        e => e.into(),
    }
}

/// Политика общего доступа организации; не задана — по умолчанию.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/calendar-sharing", tag = "sharing",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = SharingPolicyBody), (status = 403, body = Problem), (status = 503, body = Problem)))]
async fn get_policy(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<SharingPolicyBody>> {
    let s = stores(&a)?;
    Ok(Json(s.policy.sharing_policy(org).await?.into()))
}

/// Задаёт политику общего доступа организации целиком. Неизвестная группа — 400. Уже выданные права не удаляются:
/// их ограничивает новая политика.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/calendar-sharing", tag = "sharing",
    params(("org" = String, Path, description = "Организация (UUID)")),
    request_body = SharingPolicyBody,
    responses((status = 200, body = SharingPolicyBody), (status = 400, body = Problem), (status = 403, body = Problem),
        (status = 503, body = Problem)))]
async fn put_policy(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Json(body): Json<SharingPolicyBody>,
) -> ApiResult<Json<SharingPolicyBody>> {
    let s = stores(&a)?;
    if let SharingScopeBody::Groups { groups } = &body.scope {
        crate::groups::check_ids(&a, org, Vec::new(), groups.clone()).await?;
    }
    s.policy
        .set_sharing_policy(org, body.clone().into())
        .await?;
    Ok(Json(body))
}

/// Представители учётной записи и их учётные записи.
async fn delegate_infos(
    a: &Admin,
    owner: AccountScope,
    list: Vec<Delegate>,
) -> Result<Vec<DelegateInfo>> {
    let ids: Vec<AccountId> = list.iter().map(|d| d.delegate).collect();
    let accounts = a.d.accounts.by_ids(owner.tenant, &ids).await?;
    Ok(list
        .into_iter()
        .filter_map(|d| {
            let acc = accounts.iter().find(|x| x.id == d.delegate)?;
            Some(DelegateInfo {
                delegate: d.delegate.to_string(),
                login: acc.login.clone(),
                display_name: acc.display_name.clone(),
                level: d.level.into(),
                schedule_on_behalf: d.schedule_on_behalf,
            })
        })
        .collect())
}

/// Представители учётной записи.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/delegates", tag = "sharing",
    params(("org" = String, Path, description = "Организация (UUID)"), ("account" = String, Path, description = "Учётная запись (UUID)")),
    responses((status = 200, body = Vec<DelegateInfo>), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn list_delegates(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<Json<Vec<DelegateInfo>>> {
    let s = stores(&a)?;
    account(&a, org, id).await?;
    let owner = AccountScope {
        tenant: org,
        account: id,
    };
    let list = s.delegates.delegates(owner).await?;
    Ok(Json(delegate_infos(&a, owner, list).await?))
}

/// Назначает представителя или заменяет его уровень. Нет учётной записи или представителя — 404; сам себе — 400;
/// сверх политики общего доступа — 409.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/accounts/{account}/delegates/{delegate}", tag = "sharing",
    params(("org" = String, Path, description = "Организация (UUID)"), ("account" = String, Path, description = "Учётная запись (UUID)"),
        ("delegate" = String, Path, description = "Учётная запись представителя (UUID)")),
    request_body = DelegateBody,
    responses((status = 200, body = DelegateInfo), (status = 400, body = Problem), (status = 404, body = Problem),
        (status = 409, body = Problem), (status = 503, body = Problem)))]
async fn put_delegate(
    State(a): State<Arc<Admin>>,
    Path((org, id, delegate)): Path<(TenantId, AccountId, AccountId)>,
    Json(body): Json<DelegateBody>,
) -> std::result::Result<Json<DelegateInfo>, Refused> {
    let s = stores(&a)?;
    if delegate == id {
        return Err(Error::invalid("представитель — сам владелец").into());
    }
    let owner = account(&a, org, id).await?;
    let acc = account(&a, org, delegate).await?;
    let d = Delegate {
        delegate,
        level: body.level.into(),
        schedule_on_behalf: body.schedule_on_behalf,
    };
    authorizer(&a, s)
        .check_delegate(&as_owner(&owner), id, &d)
        .await
        .map_err(|e| refused(e, &acc.login))?;
    let owner = AccountScope {
        tenant: org,
        account: id,
    };
    s.delegates.set_delegate(owner, d).await?;
    Ok(Json(DelegateInfo {
        delegate: delegate.to_string(),
        login: acc.login,
        display_name: acc.display_name,
        level: body.level,
        schedule_on_behalf: body.schedule_on_behalf,
    }))
}

/// Отзывает представителя; не назначен — без изменений. Нет учётной записи — 404.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/accounts/{account}/delegates/{delegate}", tag = "sharing",
    params(("org" = String, Path, description = "Организация (UUID)"), ("account" = String, Path, description = "Учётная запись (UUID)"),
        ("delegate" = String, Path, description = "Учётная запись представителя (UUID)")),
    responses((status = 204), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn remove_delegate(
    State(a): State<Arc<Admin>>,
    Path((org, id, delegate)): Path<(TenantId, AccountId, AccountId)>,
) -> ApiResult<StatusCode> {
    let s = stores(&a)?;
    account(&a, org, id).await?;
    let owner = AccountScope {
        tenant: org,
        account: id,
    };
    s.delegates.remove_delegate(owner, delegate).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Вид коллекции.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CollectionKindBody {
    /// Календарь.
    Calendar,
    /// Адресная книга.
    AddressBook,
}

/// Календарь или адресная книга учётной записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CollectionInfo {
    /// Идентификатор (UUID).
    pub id: String,
    /// Вид.
    pub kind: CollectionKindBody,
    /// Имя — сегмент URL CalDAV/CardDAV.
    pub name: String,
    /// Отображаемое имя.
    pub display_name: String,
}

/// Календари, затем адресные книги учётной записи (по порядку в клиенте); книга GAL организации не входит — её
/// права не выдаются. Нет учётной записи — 404.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/collections", tag = "sharing",
    params(("org" = String, Path, description = "Организация (UUID)"), ("account" = String, Path, description = "Учётная запись (UUID)")),
    responses((status = 200, body = Vec<CollectionInfo>), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn list_collections(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<Json<Vec<CollectionInfo>>> {
    let s = stores(&a)?;
    account(&a, org, id).await?;
    let scope = AccountScope {
        tenant: org,
        account: id,
    };
    let mut out = Vec::new();
    for (kind, body) in [
        (CollectionKind::Calendar, CollectionKindBody::Calendar),
        (CollectionKind::AddressBook, CollectionKindBody::AddressBook),
    ] {
        for c in s.collections.list_collections(scope, kind).await? {
            if !c.is_gal() {
                out.push(CollectionInfo {
                    id: c.id.to_string(),
                    kind: body,
                    name: c.name,
                    display_name: c.display_name,
                });
            }
        }
    }
    Ok(Json(out))
}

/// Записи прав на коллекцию учётной записи.
async fn acl_entries(
    s: &SharingStores,
    owner: AccountScope,
    collection: CollectionId,
) -> Result<Vec<AclEntryBody>> {
    Ok(s.acl
        .collection_acl(owner, collection)
        .await?
        .into_iter()
        .map(AclEntryBody::from)
        .collect())
}

/// Права на календарь или адресную книгу учётной записи. Нет учётной записи или коллекции — 404.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/collections/{collection}/acl", tag = "sharing",
    params(("org" = String, Path, description = "Организация (UUID)"), ("account" = String, Path, description = "Учётная запись (UUID)"),
        ("collection" = String, Path, description = "Календарь или адресная книга (UUID)")),
    responses((status = 200, body = Vec<AclEntryBody>), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn get_acl(
    State(a): State<Arc<Admin>>,
    Path((org, id, collection)): Path<(TenantId, AccountId, CollectionId)>,
) -> ApiResult<Json<Vec<AclEntryBody>>> {
    let s = stores(&a)?;
    account(&a, org, id).await?;
    let owner = AccountScope {
        tenant: org,
        account: id,
    };
    Ok(Json(acl_entries(s, owner, collection).await?))
}

/// Заменяет права субъекта на коллекцию целиком; пустые права (`{}`) отзывают запись. Субъект — имя входа, имя
/// группы или `anyone`; недопустимый идентификатор — 400; нет учётной записи или коллекции — 404; сверх политики
/// общего доступа — 409. Ответ — записи коллекции после изменения.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/accounts/{account}/collections/{collection}/acl/{identifier}", tag = "sharing",
    params(("org" = String, Path, description = "Организация (UUID)"), ("account" = String, Path, description = "Учётная запись (UUID)"),
        ("collection" = String, Path, description = "Календарь или адресная книга (UUID)"),
        ("identifier" = String, Path, description = "Субъект: имя входа, имя группы или `anyone`")),
    request_body = RightsBody,
    responses((status = 200, body = Vec<AclEntryBody>), (status = 400, body = Problem), (status = 404, body = Problem),
        (status = 409, body = Problem), (status = 503, body = Problem)))]
async fn put_acl(
    State(a): State<Arc<Admin>>,
    Path((org, id, collection, identifier)): Path<(TenantId, AccountId, CollectionId, String)>,
    Json(body): Json<RightsBody>,
) -> std::result::Result<Json<Vec<AclEntryBody>>, Refused> {
    let s = stores(&a)?;
    validate_identifier(&identifier, false)?;
    let acc = account(&a, org, id).await?;
    let owner = AccountScope {
        tenant: org,
        account: id,
    };
    // Нет коллекции — 404 раньше проверки политики.
    s.acl.collection_acl(owner, collection).await?;
    let identifier = stored(&a, org, identifier).await?;
    let rights = CollectionRights::from(body);
    authorizer(&a, s)
        .check_grant(&as_owner(&acc), id, collection, &identifier, rights)
        .await
        .map_err(|e| refused(e, &identifier))?;
    s.acl
        .set_collection_acl(owner, collection, &identifier, rights)
        .await?;
    Ok(Json(acl_entries(s, owner, collection).await?))
}

/// Идентификатор, как хранится: `anyone` и имя входа учётной записи — в хранимом виде (без учёта регистра), иное
/// (имя группы) — как есть.
async fn stored(a: &Admin, org: TenantId, identifier: String) -> Result<String> {
    if identifier.eq_ignore_ascii_case(ANYONE) {
        return Ok(ANYONE.to_owned());
    }
    Ok(match a.d.accounts.by_login(org, &identifier).await? {
        Some(acc) => acc.login,
        None => identifier,
    })
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;
    use tp_contracts::dav::Collection;
    use tp_contracts::groups::{DistributionGroup, GroupMember, GroupStore, SenderRestriction};

    use super::*;
    use crate::testenv::Env;

    fn org() -> String {
        format!("/api/v1/orgs/{}", TenantId::DEFAULT)
    }

    async fn group(env: &Env, name: &str, members: Vec<GroupMember>) -> GroupId {
        let g = DistributionGroup {
            id: GroupId::new(),
            tenant: TenantId::DEFAULT,
            name: name.into(),
            address: None,
            external_id: None,
            members,
            senders: SenderRestriction::Anyone,
            moderators: Vec::new(),
            hidden_from_gal: false,
        };
        env.groups.put(g.clone()).await.expect("group");
        g.id
    }

    /// Приёмка ADM-16: политика общего доступа — по умолчанию, замена, неизвестная группа, роли, аудит.
    #[tokio::test]
    async fn sharing_policy() {
        let env = Env::new();
        let url = format!("{}/calendar-sharing", org());
        let (s, _, b) = env.call("GET", &url, Some("operator"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            b,
            json!({"scope": {"type": "organization"},
                   "max_outside": {"free_busy": true, "read": true, "write": false, "schedule": false, "admin": false}})
        );
        let staff = group(&env, "staff", Vec::new()).await;
        let body = json!({"scope": {"type": "groups", "groups": [staff]}, "max_outside": {"free_busy": true}});
        let (s, _, _) = env
            .call("PUT", &url, Some("operator"), Some(body.clone()))
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, b) = env.call("PUT", &url, Some("org"), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (_, _, b) = env.call("GET", &url, Some("org"), None).await;
        assert_eq!(b["scope"], json!({"type": "groups", "groups": [staff]}));
        assert_eq!(b["max_outside"]["free_busy"], true);
        assert_eq!(b["max_outside"]["read"], false);
        for bad in [
            json!({"scope": {"type": "groups", "groups": [GroupId::new()]}}),
            json!({"scope": {"type": "everyone"}}),
            json!({"max_outside": {"read": true, "delete": true}}),
        ] {
            let (s, _, b) = env.call("PUT", &url, Some("org"), Some(bad.clone())).await;
            assert!(
                s.is_client_error() && s != StatusCode::FORBIDDEN,
                "{bad}: {s} {b}"
            );
        }
        let log = env.audit.records();
        assert!(
            log.iter().any(|r| r.route == POLICY && r.status == 200),
            "{log:?}"
        );
        assert!(
            log.iter().any(|r| r.route == POLICY && r.status == 403),
            "{log:?}"
        );
    }

    /// Приёмка ADM-16: представители — назначение, замена, список, отзыв; ограничение политикой.
    #[tokio::test]
    async fn delegates() {
        let env = Env::new();
        let boss = env.account("boss@example.org").await;
        let aide = env.account("aide@example.org").await;
        let base = format!("{}/accounts/{}/delegates", org(), boss.account);
        let one = format!("{base}/{}", aide.account);
        let (s, _, b) = env.call("GET", &base, Some("org"), None).await;
        assert_eq!((s, b), (StatusCode::OK, json!([])));

        let (s, _, b) = env
            .call("PUT", &one, Some("org"), Some(json!({"level": "read"})))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["login"], "aide@example.org");
        assert_eq!(b["schedule_on_behalf"], false);
        let (s, _, b) = env
            .call(
                "PUT",
                &one,
                Some("org"),
                Some(json!({"level": "write", "schedule_on_behalf": true})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (_, _, b) = env.call("GET", &base, Some("operator"), None).await;
        assert_eq!(
            b,
            json!([{"delegate": aide.account.to_string(), "login": "aide@example.org",
                    "display_name": "aide@example.org", "level": "write", "schedule_on_behalf": true}])
        );

        // Сам себе — 400; нет представителя — 404; неверное тело — клиентская ошибка.
        let me = format!("{base}/{}", boss.account);
        let (s, _, _) = env
            .call("PUT", &me, Some("org"), Some(json!({"level": "read"})))
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let nobody = format!("{base}/{}", AccountId::new());
        let (s, _, _) = env
            .call("PUT", &nobody, Some("org"), Some(json!({"level": "read"})))
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = env
            .call("PUT", &one, Some("org"), Some(json!({"level": "owner"})))
            .await;
        assert!(s.is_client_error());

        // Общий доступ запрещён политикой — назначение 409, отзыв разрешён.
        let policy = format!("{}/calendar-sharing", org());
        env.call(
            "PUT",
            &policy,
            Some("org"),
            Some(json!({"scope": {"type": "nobody"}})),
        )
        .await;
        let (s, _, b) = env
            .call("PUT", &one, Some("org"), Some(json!({"level": "read"})))
            .await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(
            b["detail"]
                .as_str()
                .is_some_and(|d| d.contains("calendar-sharing")),
            "{b}"
        );
        let (s, _, _) = env.call("DELETE", &one, Some("operator"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env.call("DELETE", &one, Some("org"), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, _, b) = env.call("GET", &base, Some("org"), None).await;
        assert_eq!(b, json!([]));

        // Чужая организация — 403; нет учётной записи — 404.
        let (s, _, _) = env.call("GET", &base, Some("other"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let missing = format!("{}/accounts/{}/delegates", org(), AccountId::new());
        let (s, _, _) = env.call("GET", &missing, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    /// Приёмка ADM-16: права на коллекцию — субъекты, отзыв, ограничение политикой с группами.
    #[tokio::test]
    async fn collection_acl() {
        let env = Env::new();
        let owner = env.account("owner@example.org").await;
        let peer = env.account("peer@example.org").await;
        env.account("out@example.org").await;
        let work = Collection::new(CollectionKind::Calendar, "work");
        env.dav
            .create_collection(owner, work.clone())
            .await
            .expect("collection");
        let collection = work.id;
        let (s, _, b) = env
            .call(
                "GET",
                &format!("{}/accounts/{}/collections", org(), owner.account),
                Some("operator"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            b,
            json!([{"id": collection.to_string(), "kind": "calendar", "name": "work", "display_name": "work"}])
        );
        let base = format!(
            "{}/accounts/{}/collections/{collection}/acl",
            org(),
            owner.account
        );
        let (s, _, b) = env.call("GET", &base, Some("operator"), None).await;
        assert_eq!((s, b), (StatusCode::OK, json!([])));

        // Имя входа приводится к хранимому; `anyone` — не больше чтения по умолчанию.
        let (s, _, b) = env
            .call(
                "PUT",
                &format!("{base}/Peer@Example.org"),
                Some("org"),
                Some(json!({"write": true, "schedule": true})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b[0]["identifier"], "peer@example.org");
        assert_eq!(b[0]["rights"]["write"], true);
        assert_eq!(b[0]["rights"]["read"], false, "как выданы, без младших");
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{base}/anyone"),
                Some("org"),
                Some(json!({"write": true})),
            )
            .await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _, b) = env
            .call(
                "PUT",
                &format!("{base}/anyone"),
                Some("org"),
                Some(json!({"read": true})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b.as_array().map(Vec::len), Some(2));

        // Полные права — только группе staff (участникам — с вложенной team), остальным — занятость; субъект-группа —
        // по имени группы политики (единый слой прав tp-auth).
        let team = group(&env, "team", vec![GroupMember::Account(peer.account)]).await;
        let staff = group(&env, "staff", vec![GroupMember::Group(team)]).await;
        let policy = format!("{}/calendar-sharing", org());
        env.call(
            "PUT",
            &policy,
            Some("org"),
            Some(json!({"scope": {"type": "groups", "groups": [staff]}, "max_outside": {"free_busy": true}})),
        )
        .await;
        let admin = json!({"admin": true});
        for (who, status) in [
            ("peer@example.org", StatusCode::OK),
            ("staff", StatusCode::OK),
            ("Staff", StatusCode::OK),
            ("team", StatusCode::CONFLICT),
            ("out@example.org", StatusCode::CONFLICT),
            ("ldap-group", StatusCode::CONFLICT),
        ] {
            let (s, _, b) = env
                .call(
                    "PUT",
                    &format!("{base}/{who}"),
                    Some("org"),
                    Some(admin.clone()),
                )
                .await;
            assert_eq!(s, status, "{who}: {b}");
        }
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{base}/out@example.org"),
                Some("org"),
                Some(json!({"free_busy": true})),
            )
            .await;
        assert_eq!(s, StatusCode::OK);

        // Отзыв — пустые права; уже выданное `anyone` политика не удаляет.
        let (s, _, b) = env
            .call(
                "PUT",
                &format!("{base}/staff"),
                Some("org"),
                Some(json!({})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let ids: Vec<&str> = b
            .as_array()
            .expect("entries")
            .iter()
            .filter_map(|e| e["identifier"].as_str())
            .collect();
        assert!(
            ids.contains(&"anyone") && !ids.contains(&"staff"),
            "{ids:?}"
        );

        // Недопустимый идентификатор — 400; нет коллекции — 404; оператор не меняет.
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{base}/-peer"),
                Some("org"),
                Some(json!({"read": true})),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let missing = format!(
            "{}/accounts/{}/collections/{}/acl",
            org(),
            owner.account,
            CollectionId::new()
        );
        let (s, _, _) = env.call("GET", &missing, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{missing}/anyone"),
                Some("org"),
                Some(json!({"read": true})),
            )
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{base}/anyone"),
                Some("operator"),
                Some(json!({})),
            )
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(
            env.audit
                .records()
                .iter()
                .any(|r| r.route == ACL_ENTRY && r.status == 409)
        );
    }

    /// Без хранилищ общего доступа — 503.
    #[tokio::test]
    async fn without_stores() {
        let env = Env::without_sharing();
        let acc = env.account("a@example.org").await;
        for url in [
            format!("{}/calendar-sharing", org()),
            format!("{}/accounts/{}/delegates", org(), acc.account),
        ] {
            let (s, _, b) = env.call("GET", &url, Some("org"), None).await;
            assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{url}: {b}");
        }
        let (s, _, _) = env
            .call("GET", &format!("{}/resources", org()), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Схема OpenAPI содержит операции ADM-16.
    #[test]
    fn operations_in_openapi() {
        let spec = serde_json::to_value(crate::server::openapi_spec()).expect("spec");
        for (path, methods) in [
            (POLICY, &["get", "put"][..]),
            (DELEGATES, &["get"]),
            (DELEGATE, &["put", "delete"]),
            (COLLECTIONS, &["get"]),
            (ACL, &["get"]),
            (ACL_ENTRY, &["put"]),
            ("/api/v1/orgs/{org}/resources", &["get", "post"]),
            ("/api/v1/orgs/{org}/resources/{account}", &["get", "delete"]),
            (
                "/api/v1/orgs/{org}/resources/{account}/booking-policy",
                &["put"],
            ),
        ] {
            for m in methods {
                assert!(spec["paths"][path][m].is_object(), "{m} {path}");
            }
        }
    }
}
