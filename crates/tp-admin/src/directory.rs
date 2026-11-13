//! Адресная книга организации (SRS 4.5; решение 2026-11-04-exchange-parity; контракты CTR-35 `tp_contracts::addressbook`):
//!
//! - почтовые контакты (`/api/v1/orgs/{org}/contacts`): список, создание, чтение, замена (`PUT`), удаление. Контакты
//!   каталога (`external_id`) ведёт синхронизация: изменить или удалить их через API нельзя (409);
//! - дерево подразделений (`/api/v1/orgs/{org}/units`): плоский список с `parent` (корень — `null`), создание, замена,
//!   удаление (есть вложенные — 409), учётные записи подразделения (`subtree` — с вложенными) по старшинству;
//!   подразделения каталога (`external_id`) — только чтение;
//! - динамические группы (`/api/v1/orgs/{org}/groups/{group}/filter`): фильтр состава группы рассылки по атрибутам
//!   каталога и подразделениям; у группы со статическими участниками фильтр не задаётся (409). Состав вычисляется при
//!   раскрытии — `GET …/filter/members`.
//!
//! Хранилища — поля [`Deps`] `contacts`, `org`, `dynamic`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use serde::{Deserialize, Serialize};
use tp_contracts::addressbook::{DynamicGroup, MailContact, MemberFilter, OrgMember, OrgUnit};
use tp_contracts::{ContactId, Error, GroupId, OrgUnitId, Result, TenantId};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::extract::{Json, Path, Query};
use crate::server::{Admin, ApiResult, Deps, Problem};

/// Почтовый контакт.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ContactInfo {
    /// Идентификатор.
    pub id: String,
    /// Отображаемое имя.
    pub display_name: String,
    /// Внешний адрес.
    pub email: String,
    /// Идентификатор в каталоге; `null` — создан через API.
    pub external_id: Option<String>,
    /// Атрибуты адресной книги и фильтров.
    pub attributes: BTreeMap<String, String>,
    /// Группы рассылки, в составе которых контакт.
    #[schema(value_type = Vec<String>)]
    pub groups: Vec<GroupId>,
    /// Скрыт из GAL.
    pub hidden_from_gal: bool,
}

impl From<MailContact> for ContactInfo {
    fn from(c: MailContact) -> Self {
        Self {
            id: c.id.to_string(),
            display_name: c.display_name,
            email: c.email,
            external_id: c.external_id,
            attributes: c.attributes,
            groups: c.groups,
            hidden_from_gal: c.hidden_from_gal,
        }
    }
}

/// Контакт без идентификатора: создание и замена целиком.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContactBody {
    /// Отображаемое имя (1–255 символов без управляющих).
    pub display_name: String,
    /// Внешний адрес; уникален в организации.
    pub email: String,
    /// Атрибуты (имя ASCII → значение).
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    /// Группы рассылки организации.
    #[serde(default)]
    #[schema(value_type = Vec<String>)]
    pub groups: Vec<GroupId>,
    /// Скрыт из GAL.
    #[serde(default)]
    pub hidden_from_gal: bool,
}

/// Подразделение.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UnitInfo {
    /// Идентификатор.
    pub id: String,
    /// Название.
    pub name: String,
    /// Вышестоящее подразделение; `null` — корневое.
    pub parent: Option<String>,
    /// Идентификатор в каталоге; `null` — создано через API.
    pub external_id: Option<String>,
}

impl From<OrgUnit> for UnitInfo {
    fn from(u: OrgUnit) -> Self {
        Self {
            id: u.id.to_string(),
            name: u.name,
            parent: u.parent.map(|p| p.to_string()),
            external_id: u.external_id,
        }
    }
}

/// Подразделение без идентификатора: создание и замена целиком.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitBody {
    /// Название (1–255 символов без управляющих).
    pub name: String,
    /// Вышестоящее подразделение; нет — корневое.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub parent: Option<OrgUnitId>,
}

/// Учётная запись подразделения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UnitMember {
    /// Учётная запись.
    pub account: String,
    /// Подразделение.
    pub department: Option<String>,
    /// Должность.
    pub title: Option<String>,
    /// Руководитель.
    pub manager: Option<String>,
    /// Старшинство (больше — выше).
    pub seniority: Option<i32>,
}

impl From<OrgMember> for UnitMember {
    fn from(m: OrgMember) -> Self {
        Self {
            account: m.account.to_string(),
            department: m.org.department.map(|d| d.to_string()),
            title: m.org.title,
            manager: m.org.manager.map(|x| x.to_string()),
            seniority: m.org.seniority,
        }
    }
}

/// Выбор учётных записей подразделения.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct MembersQuery {
    /// С вложенными подразделениями.
    #[serde(default)]
    pub subtree: bool,
}

/// Фильтр динамической группы.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FilterBody {
    /// Условие состава: `{"equals": {"attribute", "value"}}`, `{"prefix": {...}}`, `{"present": {"attribute"}}`,
    /// `{"unit": {"unit", "subtree"}}`, `{"all": [...]}`, `{"any": [...]}`, `{"not": {...}}`.
    #[schema(value_type = Object)]
    pub filter: MemberFilter,
    /// Включать почтовые контакты, подходящие под фильтр.
    #[serde(default)]
    pub contacts: bool,
}

/// Состав динамической группы.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DynamicMembers {
    /// Учётные записи.
    pub accounts: Vec<String>,
    /// Почтовые контакты.
    pub contacts: Vec<String>,
}

/// Маршруты адресной книги.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route(
        "/api/v1/orgs/{org}/contacts",
        get(list_contacts).post(create_contact),
    )
    .route(
        "/api/v1/orgs/{org}/contacts/{contact}",
        get(get_contact).put(put_contact).delete(delete_contact),
    )
    .route(
        "/api/v1/orgs/{org}/units",
        get(list_units).post(create_unit),
    )
    .route(
        "/api/v1/orgs/{org}/units/{unit}",
        axum::routing::put(put_unit).delete(delete_unit),
    )
    .route("/api/v1/orgs/{org}/units/{unit}/members", get(unit_members))
    .route(
        "/api/v1/orgs/{org}/groups/{group}/filter",
        get(get_filter).put(put_filter).delete(delete_filter),
    )
    .route(
        "/api/v1/orgs/{org}/groups/{group}/filter/members",
        get(filter_members),
    )
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(
        list_contacts,
        create_contact,
        get_contact,
        put_contact,
        delete_contact,
        list_units,
        create_unit,
        put_unit,
        delete_unit,
        unit_members,
        get_filter,
        put_filter,
        delete_filter,
        filter_members
    ),
    components(schemas(
        ContactInfo,
        ContactBody,
        UnitInfo,
        UnitBody,
        UnitMember,
        FilterBody,
        DynamicMembers
    ))
)]
pub(crate) struct Doc;

async fn contact(s: &Deps, org: TenantId, id: ContactId) -> Result<MailContact> {
    s.contacts.contact(org, id).await?.ok_or(Error::NotFound)
}

/// Группы контакта существуют в организации; иначе — `Invalid`.
async fn check_groups(a: &Admin, org: TenantId, groups: &[GroupId]) -> Result<()> {
    for g in groups {
        if a.d.groups.get(org, *g).await?.is_none() {
            return Err(Error::invalid(format!("contact: unknown group {g}")));
        }
    }
    Ok(())
}

/// Почтовые контакты организации.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/contacts", tag = "addressbook",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = Vec<ContactInfo>)))]
async fn list_contacts(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<Vec<ContactInfo>>> {
    let list = a.d.contacts.contacts(org).await?;
    Ok(Json(list.into_iter().map(ContactInfo::from).collect()))
}

/// Создаёт локальный почтовый контакт.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/contacts", tag = "addressbook",
    params(("org" = String, Path, description = "Организация (UUID)")),
    request_body = ContactBody,
    responses((status = 201, body = ContactInfo), (status = 400, body = Problem),
        (status = 409, description = "адрес занят", body = Problem)))]
async fn create_contact(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Json(body): Json<ContactBody>,
) -> ApiResult<(StatusCode, Json<ContactInfo>)> {
    let s = &a.d;
    let c = build_contact(&a, org, ContactId::new(), None, body).await?;
    s.contacts.put_contact(c.clone()).await?;
    Ok((StatusCode::CREATED, Json(c.into())))
}

async fn build_contact(
    a: &Admin,
    org: TenantId,
    id: ContactId,
    external_id: Option<String>,
    body: ContactBody,
) -> Result<MailContact> {
    check_groups(a, org, &body.groups).await?;
    let mut groups = body.groups;
    groups.sort_unstable();
    groups.dedup();
    let c = MailContact {
        id,
        tenant: org,
        display_name: body.display_name,
        email: body.email,
        external_id,
        attributes: body.attributes,
        groups,
        hidden_from_gal: body.hidden_from_gal,
    };
    c.validate()?;
    Ok(c)
}

/// Почтовый контакт.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/contacts/{contact}", tag = "addressbook",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("contact" = String, Path, description = "Контакт (UUID)")),
    responses((status = 200, body = ContactInfo), (status = 404, body = Problem)))]
async fn get_contact(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, ContactId)>,
) -> ApiResult<Json<ContactInfo>> {
    Ok(Json(contact(&a.d, org, id).await?.into()))
}

/// Заменяет локальный контакт целиком; контакт каталога — 409.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/contacts/{contact}", tag = "addressbook",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("contact" = String, Path, description = "Контакт (UUID)")),
    request_body = ContactBody,
    responses((status = 200, body = ContactInfo), (status = 400, body = Problem), (status = 404, body = Problem),
        (status = 409, description = "контакт каталога или адрес занят", body = Problem)))]
async fn put_contact(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, ContactId)>,
    Json(body): Json<ContactBody>,
) -> ApiResult<Json<ContactInfo>> {
    let s = &a.d;
    if contact(s, org, id).await?.external_id.is_some() {
        return Err(Error::Conflict.into());
    }
    let c = build_contact(&a, org, id, None, body).await?;
    s.contacts.put_contact(c.clone()).await?;
    Ok(Json(c.into()))
}

/// Удаляет локальный контакт; контакт каталога — 409.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/contacts/{contact}", tag = "addressbook",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("contact" = String, Path, description = "Контакт (UUID)")),
    responses((status = 204), (status = 404, body = Problem),
        (status = 409, description = "контакт каталога", body = Problem)))]
async fn delete_contact(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, ContactId)>,
) -> ApiResult<StatusCode> {
    let s = &a.d;
    if contact(s, org, id).await?.external_id.is_some() {
        return Err(Error::Conflict.into());
    }
    s.contacts.remove_contact(org, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Подразделения организации (дерево — по `parent`).
#[utoipa::path(get, path = "/api/v1/orgs/{org}/units", tag = "addressbook",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = Vec<UnitInfo>)))]
async fn list_units(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<Vec<UnitInfo>>> {
    let list = a.d.org.org_units(org).await?;
    Ok(Json(list.into_iter().map(UnitInfo::from).collect()))
}

/// Создаёт подразделение.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/units", tag = "addressbook",
    params(("org" = String, Path, description = "Организация (UUID)")),
    request_body = UnitBody,
    responses((status = 201, body = UnitInfo), (status = 400, body = Problem)))]
async fn create_unit(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Json(body): Json<UnitBody>,
) -> ApiResult<(StatusCode, Json<UnitInfo>)> {
    let unit = OrgUnit {
        id: OrgUnitId::new(),
        tenant: org,
        name: body.name,
        parent: body.parent,
        external_id: None,
    };
    a.d.org.put_org_unit(unit.clone()).await?;
    Ok((StatusCode::CREATED, Json(unit.into())))
}

async fn unit(s: &Deps, org: TenantId, id: OrgUnitId) -> Result<OrgUnit> {
    s.org
        .org_units(org)
        .await?
        .into_iter()
        .find(|u| u.id == id)
        .ok_or(Error::NotFound)
}

/// Заменяет локальное подразделение (название, вышестоящее); подразделение каталога — 409.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/units/{unit}", tag = "addressbook",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("unit" = String, Path, description = "Подразделение (UUID)")),
    request_body = UnitBody,
    responses((status = 200, body = UnitInfo), (status = 400, description = "цикл, неизвестное вышестоящее",
        body = Problem), (status = 404, body = Problem),
        (status = 409, description = "подразделение каталога", body = Problem)))]
async fn put_unit(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, OrgUnitId)>,
    Json(body): Json<UnitBody>,
) -> ApiResult<Json<UnitInfo>> {
    let s = &a.d;
    if unit(s, org, id).await?.external_id.is_some() {
        return Err(Error::Conflict.into());
    }
    let u = OrgUnit {
        id,
        tenant: org,
        name: body.name,
        parent: body.parent,
        external_id: None,
    };
    s.org.put_org_unit(u.clone()).await?;
    Ok(Json(u.into()))
}

/// Удаляет локальное подразделение; есть вложенные или подразделение каталога — 409.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/units/{unit}", tag = "addressbook",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("unit" = String, Path, description = "Подразделение (UUID)")),
    responses((status = 204), (status = 404, body = Problem),
        (status = 409, description = "есть вложенные или подразделение каталога", body = Problem)))]
async fn delete_unit(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, OrgUnitId)>,
) -> ApiResult<StatusCode> {
    let s = &a.d;
    if unit(s, org, id).await?.external_id.is_some() {
        return Err(Error::Conflict.into());
    }
    s.org.remove_org_unit(org, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Учётные записи подразделения по старшинству.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/units/{unit}/members", tag = "addressbook",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("unit" = String, Path, description = "Подразделение (UUID)"), MembersQuery),
    responses((status = 200, body = Vec<UnitMember>), (status = 404, body = Problem)))]
async fn unit_members(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, OrgUnitId)>,
    Query(q): Query<MembersQuery>,
) -> ApiResult<Json<Vec<UnitMember>>> {
    let list = a.d.org.org_members(org, Some(id), q.subtree).await?;
    Ok(Json(list.into_iter().map(UnitMember::from).collect()))
}

/// Фильтр динамической группы; группа не динамическая — 404.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/groups/{group}/filter", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("group" = String, Path, description = "Группа (UUID)")),
    responses((status = 200, body = FilterBody), (status = 404, body = Problem)))]
async fn get_filter(
    State(a): State<Arc<Admin>>,
    Path((org, group)): Path<(TenantId, GroupId)>,
) -> ApiResult<Json<FilterBody>> {
    let g =
        a.d.dynamic
            .dynamic(org, group)
            .await?
            .ok_or(Error::NotFound)?;
    Ok(Json(FilterBody {
        filter: g.filter,
        contacts: g.contacts,
    }))
}

/// Задаёт фильтр состава группы рассылки (группа становится динамической); у группы есть статические участники — 409.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/groups/{group}/filter", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("group" = String, Path, description = "Группа (UUID)")),
    request_body = FilterBody,
    responses((status = 200, body = FilterBody), (status = 400, body = Problem), (status = 404, body = Problem),
        (status = 409, description = "у группы есть статические участники", body = Problem)))]
async fn put_filter(
    State(a): State<Arc<Admin>>,
    Path((org, group)): Path<(TenantId, GroupId)>,
    Json(body): Json<FilterBody>,
) -> ApiResult<Json<FilterBody>> {
    let s = &a.d;
    let g = a.d.groups.get(org, group).await?.ok_or(Error::NotFound)?;
    if !g.members.is_empty() {
        return Err(Error::Conflict.into());
    }
    body.filter.validate()?;
    s.dynamic
        .put_dynamic(DynamicGroup {
            group,
            tenant: org,
            filter: body.filter.clone(),
            contacts: body.contacts,
        })
        .await?;
    Ok(Json(body))
}

/// Снимает фильтр: группа снова статическая; группа не динамическая — 404.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/groups/{group}/filter", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("group" = String, Path, description = "Группа (UUID)")),
    responses((status = 204), (status = 404, body = Problem)))]
async fn delete_filter(
    State(a): State<Arc<Admin>>,
    Path((org, group)): Path<(TenantId, GroupId)>,
) -> ApiResult<StatusCode> {
    a.d.dynamic.remove_dynamic(org, group).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Текущий состав динамической группы (вычисляется по фильтру).
#[utoipa::path(get, path = "/api/v1/orgs/{org}/groups/{group}/filter/members", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("group" = String, Path, description = "Группа (UUID)")),
    responses((status = 200, body = DynamicMembers), (status = 404, body = Problem)))]
async fn filter_members(
    State(a): State<Arc<Admin>>,
    Path((org, group)): Path<(TenantId, GroupId)>,
) -> ApiResult<Json<DynamicMembers>> {
    let m = a.d.dynamic.expand_dynamic(org, group).await?;
    Ok(Json(DynamicMembers {
        accounts: m.accounts.iter().map(ToString::to_string).collect(),
        contacts: m.contacts.iter().map(ToString::to_string).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;
    use tp_contracts::TenantId;
    use tp_contracts::addressbook::{OrgAttributes, OrgStore};
    use tp_contracts::groups::{DistributionGroup, GroupMember, GroupStore, SenderRestriction};
    use tp_contracts::{GroupId, OrgUnitId};

    use crate::testenv::Env;

    const ORG: &str = "/api/v1/orgs/00000000-0000-0000-0000-000000000001";

    async fn group(env: &Env, members: Vec<GroupMember>) -> GroupId {
        let id = GroupId::new();
        let g = DistributionGroup {
            id,
            tenant: TenantId::DEFAULT,
            name: "Sales".into(),
            address: Some(format!("g-{id}@example.org")),
            external_id: None,
            members,
            senders: SenderRestriction::Anyone,
            moderators: vec![],
            hidden_from_gal: false,
        };
        env.groups.put(g.clone()).await.expect("group");
        g.id
    }

    #[tokio::test]
    async fn contacts_crud() {
        let env = Env::new();
        let g = group(&env, vec![]).await;
        let (s, _, c) = env
            .call(
                "POST",
                &format!("{ORG}/contacts"),
                Some("org"),
                Some(json!({"display_name": "Партнёр", "email": "p@partner.example", "groups": [g, g]})),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{c}");
        assert_eq!(c["groups"], json!([g]));
        let id = c["id"].as_str().expect("id").to_owned();
        let (s, _, _) = env
            .call(
                "POST",
                &format!("{ORG}/contacts"),
                Some("org"),
                Some(json!({"display_name": "Дубль", "email": "P@partner.example"})),
            )
            .await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _, _) = env
            .call(
                "POST",
                &format!("{ORG}/contacts"),
                Some("org"),
                Some(json!({"display_name": "x", "email": "y@z.example", "groups": [GroupId::new()]})),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, c) = env
            .call(
                "PUT",
                &format!("{ORG}/contacts/{id}"),
                Some("org"),
                Some(json!({"display_name": "Партнёр 2", "email": "p@partner.example", "hidden_from_gal": true})),
            )
            .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(c["hidden_from_gal"], true);
        let (_, _, list) = env
            .call("GET", &format!("{ORG}/contacts"), Some("operator"), None)
            .await;
        assert_eq!(list[0]["display_name"], "Партнёр 2");
        let (s, _, _) = env
            .call("DELETE", &format!("{ORG}/contacts/{id}"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _, _) = env
            .call("GET", &format!("{ORG}/contacts/{id}"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn directory_contact_is_read_only() {
        let env = Env::new();
        let c = tp_contracts::addressbook::MailContact {
            id: tp_contracts::ContactId::new(),
            tenant: TenantId::DEFAULT,
            display_name: "Из каталога".into(),
            email: "dir@partner.example".into(),
            external_id: Some("cn=dir".into()),
            attributes: Default::default(),
            groups: vec![],
            hidden_from_gal: false,
        };
        tp_contracts::addressbook::ContactStore::put_contact(&env.addressbook, c.clone())
            .await
            .expect("put");
        let path = format!("{ORG}/contacts/{}", c.id);
        let (s, _, _) = env.call("DELETE", &path, Some("org"), None).await;
        assert_eq!(s, StatusCode::CONFLICT);
        let body = json!({"display_name": "x", "email": "dir@partner.example"});
        let (s, _, _) = env.call("PUT", &path, Some("org"), Some(body)).await;
        assert_eq!(s, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn units_tree_and_members() {
        let env = Env::new();
        let (s, _, root) = env
            .call(
                "POST",
                &format!("{ORG}/units"),
                Some("org"),
                Some(json!({"name": "Компания"})),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED);
        let root_id = root["id"].as_str().expect("id").to_owned();
        let (_, _, child) = env
            .call(
                "POST",
                &format!("{ORG}/units"),
                Some("org"),
                Some(json!({"name": "Продажи", "parent": root_id})),
            )
            .await;
        let child_id = child["id"].as_str().expect("id").to_owned();
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{ORG}/units/{root_id}"),
                Some("org"),
                Some(json!({"name": "Компания", "parent": child_id})),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "цикл");
        let acc = env.account("seller@example.org").await;
        env.addressbook
            .set_org(
                TenantId::DEFAULT,
                acc.account,
                OrgAttributes {
                    department: Some(child_id.parse::<OrgUnitId>().expect("unit")),
                    title: Some("Менеджер".into()),
                    seniority: Some(3),
                    ..OrgAttributes::default()
                },
            )
            .await
            .expect("org");
        let (s, _, m) = env
            .call(
                "GET",
                &format!("{ORG}/units/{root_id}/members?subtree=true"),
                Some("operator"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(m[0]["account"], acc.account.to_string());
        assert_eq!(m[0]["title"], "Менеджер");
        let (_, _, m) = env
            .call(
                "GET",
                &format!("{ORG}/units/{root_id}/members"),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(m, json!([]));
        let (s, _, _) = env
            .call(
                "DELETE",
                &format!("{ORG}/units/{root_id}"),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::CONFLICT, "есть вложенные");
        let (_, _, list) = env
            .call("GET", &format!("{ORG}/units"), Some("org"), None)
            .await;
        assert_eq!(list.as_array().map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn dynamic_group_filter() {
        let env = Env::new();
        let g = group(&env, vec![]).await;
        let acc = env.account("boss@example.org").await;
        env.addressbook
            .set_org(
                TenantId::DEFAULT,
                acc.account,
                OrgAttributes {
                    title: Some("Директор".into()),
                    ..OrgAttributes::default()
                },
            )
            .await
            .expect("org");
        let path = format!("{ORG}/groups/{g}/filter");
        let (s, _, _) = env.call("GET", &path, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let filter = json!({"filter": {"equals": {"attribute": "title", "value": "директор"}}});
        let (s, _, b) = env.call("PUT", &path, Some("org"), Some(filter)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (_, _, m) = env
            .call("GET", &format!("{path}/members"), Some("org"), None)
            .await;
        assert_eq!(m["accounts"], json!([acc.account.to_string()]));
        let (s, _, _) = env.call("DELETE", &path, Some("org"), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);

        let fixed = group(&env, vec![GroupMember::Account(acc.account)]).await;
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{ORG}/groups/{fixed}/filter"),
                Some("org"),
                Some(json!({"filter": {"all": []}})),
            )
            .await;
        assert_eq!(s, StatusCode::CONFLICT);
    }
}
