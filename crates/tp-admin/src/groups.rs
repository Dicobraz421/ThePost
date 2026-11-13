//! Группы рассылки: настройки и модерация (SRS 4.5; CONTRACTS.md#Groups).
//!
//! - группы организации (`/api/v1/orgs/{org}/groups`): список, создание, чтение, изменение (`PATCH`), удаление;
//!   ограничение круга отправителей (`anyone`, `internal`, `{"only": {"accounts": [...], "groups": [...]}}`),
//!   модераторы (непусто — группа модерируемая), скрытие из GAL. У групп каталога (`external_id`, AUTH-07)
//!   имя, адрес и участники — из каталога: через API меняются только ограничения, модераторы и скрытие
//!   (если они не сопоставлены в секции каталога, иначе синхронизация их перезапишет); удалить группу каталога
//!   нельзя. В ответе — словарь `names`: имя входа каждой упомянутой учётной записи и имя каждой упомянутой
//!   группы (участники, перечисленные отправители, модераторы); имена читаются одним запросом учётных записей
//!   (`AccountStore::by_ids`) и одним списком групп, а не запросом на участника;
//! - очередь модерации (`/api/v1/orgs/{org}/moderation`): письма по времени постановки страницами (`after`,
//!   продолжение — заголовок `Link` с `rel="next"`, как у списка учётных записей), одобрение — доставка
//!   участникам, отклонение — уведомление отправителя с причиной. Письмо забирается из очереди
//!   (`GroupStore::take_held`) и передаётся [`Moderation`] (контракт, CTR-31; реализация — `tp-smtp`: раскрытие группы,
//!   приём в ящики, DSN, журнал доставки); ошибка решения возвращает его в очередь. Тема письма (`subject`) —
//!   только по `include=subject` и только администратору, с записью в аудит (`crate::content`); источник —
//!   объект `HeldMessage::blob` ([`Admin::with_held_blobs`], без него — `null`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{MatchedPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;
pub use tp_contracts::groups::Moderation;
use tp_contracts::groups::{
    DistributionGroup, GroupMember, HeldCursor, HeldMessage, HoldSource, Moderator,
    SenderRestriction, validate_reject_reason,
};
use tp_contracts::{AccountId, Error, GroupId, HeldId, Result, TenantId};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::content::{self, Access};
use crate::extract::{Json, Path, Query};
use crate::server::{Admin, ApiResult, Caller, Problem};

/// Участник группы: учётная запись или вложенная группа.
// Схема OpenAPI контракта `tp_contracts::groups::GroupMember` (в `serde` — тот же вид); значений этого типа нет.
#[allow(dead_code)]
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[schema(as = GroupMember)]
pub enum GroupMemberSchema {
    /// Учётная запись организации (UUID).
    Account(String),
    /// Вложенная группа (UUID).
    Group(String),
}

/// Кто может писать в группу: `"anyone"` — все, `"internal"` — учётные записи организации,
/// `{"only": {"accounts": [...], "groups": [...]}}` — перечисленные учётные записи и участники групп.
// Схема OpenAPI контракта `tp_contracts::groups::SenderRestriction`; значений этого типа нет.
#[allow(dead_code)]
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[schema(as = SenderRestriction)]
pub enum SenderRestrictionSchema {
    /// Без ограничений, в том числе внешние отправители.
    Anyone,
    /// Только учётные записи организации.
    Internal,
    /// Только перечисленные.
    Only {
        /// Учётные записи (UUID).
        accounts: Vec<String>,
        /// Группы (UUID).
        groups: Vec<String>,
    },
}

/// Группа рассылки.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct GroupInfo {
    /// Идентификатор.
    pub id: String,
    /// Отображаемое имя.
    pub name: String,
    /// Адрес рассылки; `null` — группа без почты (только вложенная).
    pub address: Option<String>,
    /// Идентификатор в каталоге; `null` — группа создана через Admin API.
    pub external_id: Option<String>,
    /// Участники: `{"account": UUID}` или `{"group": UUID}`.
    #[schema(value_type = Vec<GroupMemberSchema>)]
    pub members: Vec<GroupMember>,
    /// Кто может писать: `"anyone"`, `"internal"`, `{"only": {"accounts": [...], "groups": [...]}}`.
    #[schema(value_type = SenderRestrictionSchema)]
    pub senders: SenderRestriction,
    /// Модераторы; непусто — письма не от модераторов ждут одобрения.
    #[schema(value_type = Vec<String>)]
    pub moderators: Vec<AccountId>,
    /// Скрыта из адресной книги.
    pub hidden_from_gal: bool,
    /// Имена по идентификатору (UUID) для `members`, `senders` и `moderators`: учётная запись — имя входа,
    /// группа — её имя. Идентификатора нет в словаре — учётной записи или группы уже нет.
    pub names: BTreeMap<String, String>,
}

/// Идентификаторы, на которые ссылается группа: учётные записи и группы.
fn refs(g: &DistributionGroup) -> (BTreeSet<AccountId>, BTreeSet<GroupId>) {
    let mut accounts: BTreeSet<AccountId> = g.moderators.iter().copied().collect();
    let mut groups = BTreeSet::new();
    for m in &g.members {
        match m {
            GroupMember::Account(x) => accounts.insert(*x),
            GroupMember::Group(x) => groups.insert(*x),
        };
    }
    if let SenderRestriction::Only {
        accounts: sa,
        groups: sg,
    } = &g.senders
    {
        accounts.extend(sa);
        groups.extend(sg);
    }
    (accounts, groups)
}

/// Имена учётных записей и групп организации, на которые ссылаются группы ответа.
#[derive(Debug, Default)]
struct Names {
    accounts: BTreeMap<AccountId, String>,
    groups: BTreeMap<GroupId, String>,
}

impl Names {
    /// Имена для групп `shown`. Имена групп — из `all` (список групп организации) или одним `GroupStore::list`;
    /// имена входа — одним `AccountStore::by_ids` (CTR-63), без запроса на участника.
    async fn load(
        a: &Admin,
        org: TenantId,
        shown: &[DistributionGroup],
        all: Option<&[DistributionGroup]>,
    ) -> Result<Self> {
        let mut want_accounts = BTreeSet::new();
        let mut want_groups = BTreeSet::new();
        for g in shown {
            let (accounts, groups) = refs(g);
            want_accounts.extend(accounts);
            want_groups.extend(groups);
        }
        let mut names = Self::default();
        if !want_groups.is_empty() {
            let listed;
            let all = match all {
                Some(all) => all,
                None => {
                    listed = a.d.groups.list(org).await?;
                    &listed
                }
            };
            for g in all.iter().filter(|g| want_groups.contains(&g.id)) {
                names.groups.insert(g.id, g.name.clone());
            }
        }
        if !want_accounts.is_empty() {
            let ids: Vec<AccountId> = want_accounts.into_iter().collect();
            for acc in a.d.accounts.by_ids(org, &ids).await? {
                names.accounts.insert(acc.id, acc.login);
            }
        }
        Ok(names)
    }

    /// Группа с именами тех, на кого она ссылается.
    fn info(&self, g: DistributionGroup) -> GroupInfo {
        let (accounts, groups) = refs(&g);
        let accounts = accounts
            .iter()
            .filter_map(|id| Some((id.to_string(), self.accounts.get(id)?.clone())));
        let groups = groups
            .iter()
            .filter_map(|id| Some((id.to_string(), self.groups.get(id)?.clone())));
        GroupInfo {
            names: accounts.chain(groups).collect(),
            ..GroupInfo::bare(g)
        }
    }

    /// Одна группа с именами.
    async fn one(a: &Admin, g: DistributionGroup) -> Result<GroupInfo> {
        let shown = [g];
        let names = Self::load(a, shown[0].tenant, &shown, None).await?;
        let [g] = shown;
        Ok(names.info(g))
    }
}

impl GroupInfo {
    /// Группа без словаря имён.
    fn bare(g: DistributionGroup) -> Self {
        Self {
            names: BTreeMap::new(),
            id: g.id.to_string(),
            name: g.name,
            address: g.address,
            external_id: g.external_id,
            members: g.members,
            senders: g.senders,
            moderators: g.moderators,
            hidden_from_gal: g.hidden_from_gal,
        }
    }
}

/// Новая группа.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewGroup {
    /// Отображаемое имя.
    pub name: String,
    /// Адрес рассылки.
    #[serde(default)]
    pub address: Option<String>,
    /// Участники.
    #[serde(default)]
    #[schema(value_type = Vec<GroupMemberSchema>)]
    pub members: Vec<GroupMember>,
    /// Ограничение отправителей (по умолчанию — без ограничений).
    #[serde(default)]
    #[schema(value_type = SenderRestrictionSchema)]
    pub senders: SenderRestriction,
    /// Модераторы.
    #[serde(default)]
    #[schema(value_type = Vec<String>)]
    pub moderators: Vec<AccountId>,
    /// Скрыта из адресной книги.
    #[serde(default)]
    pub hidden_from_gal: bool,
}

/// `null` — значение (отличается от отсутствующего поля).
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// Изменение группы: заданные поля заменяются, отсутствующие — без изменений.
#[derive(Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GroupPatch {
    /// Отображаемое имя (не для групп каталога).
    pub name: Option<String>,
    /// Адрес рассылки; `null` — снять (не для групп каталога).
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(value_type = Option<String>)]
    pub address: Option<Option<String>>,
    /// Участники (не для групп каталога).
    #[schema(value_type = Option<Vec<GroupMemberSchema>>)]
    pub members: Option<Vec<GroupMember>>,
    /// Ограничение отправителей.
    #[schema(value_type = Option<SenderRestrictionSchema>)]
    pub senders: Option<SenderRestriction>,
    /// Модераторы.
    #[schema(value_type = Option<Vec<String>>)]
    pub moderators: Option<Vec<AccountId>>,
    /// Скрыта из адресной книги.
    pub hidden_from_gal: Option<bool>,
}

/// Письмо в очереди модерации.
#[derive(Debug, Serialize, ToSchema)]
pub struct HeldInfo {
    /// Идентификатор.
    pub id: String,
    /// Группа-адресат; `null` — письмо задержано правилом потока (CTR-43).
    pub group: Option<String>,
    /// Источник задержки: `group` — модерируемая группа, `flow_rule` — правило потока почты.
    pub source: String,
    /// Имя правила потока (для `flow_rule`).
    pub rule: Option<String>,
    /// Модераторы письма: `account:<UUID>` или `group:<UUID>`; пусто — модераторы группы.
    pub moderators: Vec<String>,
    /// Исходные получатели (для письма правила — кому доставляется одобренное письмо).
    pub recipients: Vec<String>,
    /// Отправитель конверта.
    pub sender: String,
    /// Учётная запись отправителя; `null` — внешний.
    pub sender_account: Option<String>,
    /// Message-ID.
    pub message_id: Option<String>,
    /// Размер, байт.
    pub size: u64,
    /// Момент постановки в очередь.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub held_at: OffsetDateTime,
    /// Тема письма (заголовок `Subject`, декодированный, обрезанный). Поле есть только с `include=subject`;
    /// `null` — заголовка нет или письмо недоступно.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub subject: Option<Option<String>>,
}

impl From<HeldMessage> for HeldInfo {
    fn from(h: HeldMessage) -> Self {
        let (source, rule) = match h.source {
            HoldSource::Group => ("group", None),
            HoldSource::FlowRule { rule } => ("flow_rule", Some(rule)),
        };
        Self {
            id: h.id.to_string(),
            group: h.group.map(|g| g.to_string()),
            source: source.to_owned(),
            rule,
            moderators: h
                .moderators
                .iter()
                .map(|m| match m {
                    Moderator::Account(a) => format!("account:{a}"),
                    Moderator::Group(g) => format!("group:{g}"),
                })
                .collect(),
            recipients: h.recipients,
            sender: h.sender,
            sender_account: h.sender_account.map(|a| a.to_string()),
            message_id: h.message_id,
            size: h.size,
            held_at: h.held_at,
            subject: None,
        }
    }
}

/// Очередь модерации: фильтр и размер.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct HeldQuery {
    /// Только письма группы (UUID).
    #[param(value_type = Option<String>)]
    pub group: Option<GroupId>,
    /// Размер (по умолчанию и наибольший — из конфигурации).
    pub limit: Option<usize>,
    /// Продолжение после этого письма — `after` из ссылки `rel="next"` предыдущей страницы (непрозрачная позиция).
    pub after: Option<String>,
    /// `subject` — отдать тему писем (`subject`). Только администратору; запрос пишется в журнал аудита как доступ
    /// к содержимому; размер — не больше `admin.content_page_size`.
    pub include: Option<String>,
}

/// Отклонение письма.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Rejection {
    /// Причина для уведомления отправителя (одна строка, ≤ 500 символов).
    pub reason: Option<String>,
}

/// Причина отказа по умолчанию.
pub const DEFAULT_REASON: &str = "rejected by the group moderator";

/// Маршруты групп и модерации.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route(
        "/api/v1/orgs/{org}/groups",
        get(list_groups).post(create_group),
    )
    .route(
        "/api/v1/orgs/{org}/groups/{group}",
        get(get_group).patch(patch_group).delete(delete_group),
    )
    .route("/api/v1/orgs/{org}/moderation", get(list_held))
    .route(
        "/api/v1/orgs/{org}/moderation/{held}/approve",
        post(approve),
    )
    .route("/api/v1/orgs/{org}/moderation/{held}/reject", post(reject))
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(
        list_groups,
        create_group,
        get_group,
        patch_group,
        delete_group,
        list_held,
        approve,
        reject
    ),
    components(schemas(
        GroupInfo,
        GroupMemberSchema,
        SenderRestrictionSchema,
        NewGroup,
        GroupPatch,
        HeldInfo,
        Rejection
    ))
)]
pub(crate) struct Doc;

async fn group(a: &Admin, org: TenantId, id: GroupId) -> Result<DistributionGroup> {
    a.d.groups.get(org, id).await?.ok_or(Error::NotFound)
}

/// Ссылки группы — на существующие учётные записи и группы организации; иначе `Invalid`.
async fn check_refs(a: &Admin, g: &DistributionGroup) -> Result<()> {
    let mut accounts: Vec<AccountId> = g.moderators.clone();
    let mut groups: Vec<GroupId> = Vec::new();
    for m in &g.members {
        match m {
            GroupMember::Account(x) => accounts.push(*x),
            GroupMember::Group(x) => groups.push(*x),
        }
    }
    if let SenderRestriction::Only {
        accounts: sa,
        groups: sg,
    } = &g.senders
    {
        accounts.extend(sa);
        groups.extend(sg);
    }
    groups.retain(|x| *x != g.id);
    check_ids(a, g.tenant, accounts, groups).await
}

/// Учётные записи и группы существуют в организации `tenant`; иначе `Invalid`.
pub(crate) async fn check_ids(
    a: &Admin,
    tenant: TenantId,
    mut accounts: Vec<AccountId>,
    mut groups: Vec<GroupId>,
) -> Result<()> {
    accounts.sort_unstable();
    accounts.dedup();
    groups.sort_unstable();
    groups.dedup();
    for x in accounts {
        if a.d.accounts.get(tenant, x).await?.is_none() {
            return Err(Error::invalid(format!("unknown account {x}")));
        }
    }
    for x in groups {
        if a.d.groups.get(tenant, x).await?.is_none() {
            return Err(Error::invalid(format!("unknown group {x}")));
        }
    }
    Ok(())
}

/// Группы организации.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/groups", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = Vec<GroupInfo>), (status = 403, body = Problem)))]
async fn list_groups(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<Vec<GroupInfo>>> {
    let list = a.d.groups.list(org).await?;
    let names = Names::load(&a, org, &list, Some(&list)).await?;
    Ok(Json(list.into_iter().map(|g| names.info(g)).collect()))
}

/// Создаёт группу (не из каталога).
#[utoipa::path(post, path = "/api/v1/orgs/{org}/groups", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)")),
    request_body = NewGroup,
    responses((status = 201, body = GroupInfo), (status = 400, body = Problem), (status = 409, body = Problem)))]
async fn create_group(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Json(b): Json<NewGroup>,
) -> ApiResult<(StatusCode, Json<GroupInfo>)> {
    let g = DistributionGroup {
        id: GroupId::new(),
        tenant: org,
        name: b.name,
        address: b.address,
        external_id: None,
        members: b.members,
        senders: b.senders,
        moderators: b.moderators,
        hidden_from_gal: b.hidden_from_gal,
    };
    g.validate()?;
    check_refs(&a, &g).await?;
    a.d.groups.put(g.clone()).await?;
    Ok((StatusCode::CREATED, Json(Names::one(&a, g).await?)))
}

/// Группа.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/groups/{group}", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)"), ("group" = String, Path, description = "Группа (UUID)")),
    responses((status = 200, body = GroupInfo), (status = 404, body = Problem)))]
async fn get_group(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, GroupId)>,
) -> ApiResult<Json<GroupInfo>> {
    let g = group(&a, org, id).await?;
    Ok(Json(Names::one(&a, g).await?))
}

/// Изменяет группу: ограничения, модераторы, скрытие; имя, адрес и участники — только у групп не из каталога.
#[utoipa::path(patch, path = "/api/v1/orgs/{org}/groups/{group}", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)"), ("group" = String, Path, description = "Группа (UUID)")),
    request_body = GroupPatch,
    responses((status = 200, body = GroupInfo), (status = 400, body = Problem), (status = 404, body = Problem),
        (status = 409, body = Problem)))]
async fn patch_group(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, GroupId)>,
    Json(p): Json<GroupPatch>,
) -> ApiResult<Json<GroupInfo>> {
    let mut g = group(&a, org, id).await?;
    if g.external_id.is_some() && (p.name.is_some() || p.address.is_some() || p.members.is_some()) {
        return Err(Error::invalid(
            "name, address and members of a directory group are managed by the directory",
        )
        .into());
    }
    if let Some(v) = p.name {
        g.name = v;
    }
    if let Some(v) = p.address {
        g.address = v;
    }
    if let Some(v) = p.members {
        g.members = v;
    }
    if let Some(v) = p.senders {
        g.senders = v;
    }
    if let Some(v) = p.moderators {
        g.moderators = v;
    }
    if let Some(v) = p.hidden_from_gal {
        g.hidden_from_gal = v;
    }
    g.validate()?;
    check_refs(&a, &g).await?;
    a.d.groups.put(g.clone()).await?;
    Ok(Json(Names::one(&a, g).await?))
}

/// Удаляет группу (не из каталога); письма её очереди модерации отклоняются с уведомлением.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/groups/{group}", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)"), ("group" = String, Path, description = "Группа (UUID)")),
    responses((status = 204), (status = 400, body = Problem), (status = 404, body = Problem)))]
async fn delete_group(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, GroupId)>,
) -> ApiResult<StatusCode> {
    let g = group(&a, org, id).await?;
    if g.external_id.is_some() {
        return Err(Error::invalid("a directory group is removed in the directory").into());
    }
    for h in a.d.groups.remove(org, id).await? {
        if let Err(e) =
            a.d.moderation
                .reject(org, &h, "the group was deleted")
                .await
        {
            tracing::error!(error = %e, held = %h.id, "admin: held message of deleted group not rejected");
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Очередь модерации организации или группы по времени постановки. Есть продолжение — заголовок
/// `Link: <…&after=…>; rel="next"` (RFC 8288), нет заголовка — страница последняя; неразборчивый `after` — 400.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/moderation", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)"), HeldQuery),
    responses(
        (status = 200, body = Vec<HeldInfo>, headers(("Link" = String,
            description = "Следующая страница очереди: `<ссылка>; rel=\"next\"`; нет — страница последняя"))),
        (status = 400, body = Problem), (status = 403, body = Problem)))]
async fn list_held(
    State(a): State<Arc<Admin>>,
    Extension(caller): Extension<Caller>,
    route: MatchedPath,
    uri: Uri,
    Path(org): Path<TenantId>,
    Query(q): Query<HeldQuery>,
) -> ApiResult<(HeaderMap, Json<Vec<HeldInfo>>)> {
    let subjects = content::requested(q.include.as_deref())?;
    let after = q.after.as_deref().map(parse_cursor).transpose()?;
    let access = Access {
        caller: &caller,
        org,
        route: route.as_str(),
        path: uri.path(),
    };
    if subjects {
        access.check(&a).await?;
    }
    let limit = a.cfg.content_page(q.limit, subjects);
    // На одно письмо больше: продолжение есть, только если после страницы письма действительно есть.
    let mut held =
        a.d.groups
            .held(org, q.group, after, limit.saturating_add(1))
            .await?;
    let mut headers = HeaderMap::new();
    if held.len() > limit {
        held.truncate(limit);
        if let Some(last) = held.last() {
            let link = next_link(org, q.group, subjects, &HeldCursor::of(last), limit);
            let value = HeaderValue::from_str(&link)
                .map_err(|e| Error::internal(format!("ссылка Link: {e}")))?;
            headers.insert(header::LINK, value);
        }
    }
    if !subjects {
        return Ok((headers, Json(held.into_iter().map(Into::into).collect())));
    }
    access.record(&a, 200, held.len()).await?;
    let mut out = Vec::with_capacity(held.len());
    for h in held {
        let subject = match &a.held_blobs {
            Some(blobs) => {
                let raw = blobs.get(&h.blob, Some(content::head_range(&a.cfg))).await;
                content::headline(&a.cfg, raw).await.subject
            }
            None => None,
        };
        let mut info = HeldInfo::from(h);
        info.subject = Some(subject);
        out.push(info);
    }
    Ok((headers, Json(out)))
}

/// Позиция в очереди для `after`: `<held_at, нс Unix>_<id>` — без символов, требующих кодирования в запросе.
fn cursor_text(c: &HeldCursor) -> String {
    format!("{}_{}", c.held_at.unix_timestamp_nanos(), c.id)
}

/// Разбор `after` ([`cursor_text`]); неразборчивая позиция — 400.
fn parse_cursor(s: &str) -> Result<HeldCursor> {
    let bad = || Error::invalid("after: позиция не из ссылки rel=\"next\"");
    let (at, id) = s.split_once('_').ok_or_else(bad)?;
    let held_at = at
        .parse::<i128>()
        .ok()
        .and_then(|n| OffsetDateTime::from_unix_timestamp_nanos(n).ok())
        .ok_or_else(bad)?;
    let id = id.parse::<HeldId>().map_err(|_| bad())?;
    Ok(HeldCursor { held_at, id })
}

/// Ссылка на следующую страницу очереди (RFC 8288; путь от корня Admin API) с теми же фильтром и `include`.
fn next_link(
    org: TenantId,
    group: Option<GroupId>,
    subjects: bool,
    after: &HeldCursor,
    limit: usize,
) -> String {
    let mut q = String::new();
    if let Some(g) = group {
        q.push_str(&format!("group={g}&"));
    }
    if subjects {
        q.push_str("include=subject&");
    }
    format!(
        "</api/v1/orgs/{org}/moderation?{q}limit={limit}&after={}>; rel=\"next\"",
        cursor_text(after)
    )
}

/// Возвращает письмо в очередь после неудачного решения.
async fn put_back(a: &Admin, org: TenantId, h: HeldMessage) {
    let id = h.id;
    if let Err(e) = a.d.groups.hold(org, h).await {
        tracing::error!(error = %e, held = %id, "admin: held message not returned to the queue");
    }
}

/// Одобряет письмо: доставка участникам группы.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/moderation/{held}/approve", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)"), ("held" = String, Path, description = "Письмо (UUID)")),
    responses((status = 204), (status = 404, body = Problem), (status = 409, body = Problem), (status = 503, body = Problem)))]
async fn approve(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, HeldId)>,
) -> ApiResult<StatusCode> {
    let h = a.d.groups.take_held(org, id).await?;
    let Some(gid) = h.group else {
        // Письмо правила потока (CTR-43): доставка исходным получателям.
        if let Err(e) = a.d.moderation.deliver(org, None, &h).await {
            put_back(&a, org, h).await;
            return Err(e.into());
        }
        return Ok(StatusCode::NO_CONTENT);
    };
    let g = match a.d.groups.get(org, gid).await {
        Ok(Some(g)) => g,
        Ok(None) => {
            // Группа удалена между постановкой и решением: доставлять некому.
            if let Err(e) =
                a.d.moderation
                    .reject(org, &h, "the group was deleted")
                    .await
            {
                put_back(&a, org, h).await;
                return Err(e.into());
            }
            return Err(Error::Conflict.into());
        }
        Err(e) => {
            put_back(&a, org, h).await;
            return Err(e.into());
        }
    };
    if let Err(e) = a.d.moderation.deliver(org, Some(&g), &h).await {
        put_back(&a, org, h).await;
        return Err(e.into());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Отклоняет письмо: уведомление отправителю с причиной.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/moderation/{held}/reject", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)"), ("held" = String, Path, description = "Письмо (UUID)")),
    request_body(content = Option<Rejection>),
    responses((status = 204), (status = 400, body = Problem), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn reject(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, HeldId)>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let r: Rejection = if body.is_empty() {
        Rejection::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| Error::invalid(format!("request body: {e}")))?
    };
    let reason = r.reason.unwrap_or_else(|| DEFAULT_REASON.to_owned());
    if validate_reject_reason(&reason).is_err() {
        return Err(Error::invalid("reason — one line of 1..=500 characters").into());
    }
    let h = a.d.groups.take_held(org, id).await?;
    if let Err(e) = a.d.moderation.reject(org, &h, &reason).await {
        put_back(&a, org, h).await;
        return Err(e.into());
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use time::OffsetDateTime;
    use tp_contracts::groups::{
        DistributionGroup, GroupMember, GroupStore, HeldMessage, HoldSource, Moderator,
        SenderRestriction,
    };
    use tp_contracts::{AccountId, BlobHash, BlobKey, Error, Gen, GroupId, HeldId, TenantId};

    use crate::server::AdminConfig;
    use crate::testenv::{Decision, Env, config};

    fn base() -> String {
        format!("/api/v1/orgs/{}", TenantId::DEFAULT)
    }

    fn held(group: GroupId, sender: &str) -> HeldMessage {
        HeldMessage {
            id: HeldId::new(),
            group: Some(group),
            source: HoldSource::Group,
            moderators: Vec::new(),
            recipients: Vec::new(),
            sender: sender.into(),
            sender_account: None,
            message_id: Some("m1@ext".into()),
            blob: BlobKey {
                tenant: TenantId::DEFAULT,
                hash: BlobHash([1; 32]),
                generation: Gen(1),
            },
            size: 100,
            held_at: OffsetDateTime::now_utc(),
        }
    }

    /// Путь следующей страницы из `Link: <путь>; rel="next"`; нет заголовка — `None`.
    fn next(h: &axum::http::HeaderMap) -> Option<String> {
        let v = h.get(axum::http::header::LINK)?.to_str().expect("link");
        let path = v.strip_prefix('<')?.strip_suffix(">; rel=\"next\"")?;
        Some(path.to_owned())
    }

    /// Приёмка ADM-21: очередь из 5 писем при `limit=2` читается тремя запросами по `Link` без пропусков и повторов;
    /// фильтр группы переходит в ссылку; неразборчивый `after` — 400.
    #[tokio::test]
    async fn moderation_pages() {
        let env = Env::new();
        let mut gids = Vec::new();
        for name in ["staff", "sales"] {
            let body = json!({"name": name, "address": format!("{name}@example.org")});
            let (s, _, g) = env
                .call(
                    "POST",
                    &format!("{}/groups", base()),
                    Some("org"),
                    Some(body),
                )
                .await;
            assert_eq!(s, StatusCode::CREATED, "{g}");
            let gid: GroupId = g["id"].as_str().expect("id").parse().expect("uuid");
            gids.push(gid);
        }
        let (gid, other) = (gids[0], gids[1]);
        let start = OffsetDateTime::now_utc();
        let mut queued = Vec::new();
        for i in 0..5_i64 {
            // Два письма с одним `held_at`: порядок различает `id`.
            let at = start + time::Duration::seconds(i.min(3));
            let h = HeldMessage {
                held_at: at,
                ..held(gid, &format!("s{i}@ext"))
            };
            queued.push((h.held_at, h.id));
            env.groups.hold(TenantId::DEFAULT, h).await.expect("hold");
        }
        // Письмо другой группы в выборку с фильтром не попадает.
        env.groups
            .hold(TenantId::DEFAULT, held(other, "other@ext"))
            .await
            .expect("hold");
        queued.sort();
        let expected: Vec<String> = queued.iter().map(|(_, id)| id.to_string()).collect();

        let mut url = Some(format!("{}/moderation?group={gid}&limit=2", base()));
        let (mut seen, mut requests) = (Vec::new(), 0);
        while let Some(u) = url {
            let (s, h, b) = env.call("GET", &u, Some("operator"), None).await;
            assert_eq!(s, StatusCode::OK, "{b}");
            requests += 1;
            let page = b.as_array().expect("page");
            assert!(page.len() <= 2, "{b}");
            seen.extend(
                page.iter()
                    .map(|m| m["id"].as_str().expect("id").to_owned()),
            );
            url = next(&h);
            if let Some(link) = &url {
                assert!(
                    link.contains(&format!("group={gid}&limit=2&after=")),
                    "{link}"
                );
            }
        }
        assert_eq!(requests, 3);
        assert_eq!(
            seen, expected,
            "без пропусков и повторов, по времени постановки"
        );

        for bad in ["junk", "1_x", "x_00000000-0000-0000-0000-000000000000"] {
            let u = format!("{}/moderation?after={bad}", base());
            let (s, _, _) = env.call("GET", &u, Some("operator"), None).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    /// Приёмка ADM-07: группы и их ограничения, скрытие из GAL, права.
    #[tokio::test]
    async fn groups_and_restrictions() {
        let env = Env::new();
        let alice = env.account("alice@example.org").await.account;
        let bob = env.account("bob@example.org").await.account;
        let groups = format!("{}/groups", base());
        let (s, _, inner) = env
            .call(
                "POST",
                &groups,
                Some("org"),
                Some(json!({"name": "Inner", "members": [{"account": alice}]})),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{inner}");
        let inner_id = inner["id"].as_str().expect("id").to_owned();
        let body = json!({
            "name": "Staff", "address": "staff@example.org",
            "members": [{"account": bob}, {"group": inner_id}],
            "senders": "internal", "moderators": [bob], "hidden_from_gal": true
        });
        let (s, _, g) = env.call("POST", &groups, Some("org"), Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{g}");
        assert_eq!(g["senders"], "internal");
        assert_eq!(g["hidden_from_gal"], true);
        assert_eq!(g["external_id"], Value::Null);
        let id = g["id"].as_str().expect("id").to_owned();
        let stored = env
            .groups
            .get(TenantId::DEFAULT, id.parse().expect("uuid"))
            .await
            .expect("get")
            .expect("group");
        assert_eq!(
            env.groups
                .expand(TenantId::DEFAULT, stored.id)
                .await
                .expect("expand")
                .len(),
            2,
            "вложенная группа раскрывается"
        );

        // Адрес занят, неизвестный участник, неверное имя.
        let (s, _, _) = env
            .call(
                "POST",
                &groups,
                Some("org"),
                Some(json!({"name": "Dup", "address": "STAFF@example.org"})),
            )
            .await;
        assert_eq!(s, StatusCode::CONFLICT);
        let ghost = tp_contracts::AccountId::new();
        let (s, _, b) = env
            .call(
                "POST",
                &groups,
                Some("org"),
                Some(json!({"name": "X", "members": [{"account": ghost}]})),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        let (s, _, _) = env
            .call("POST", &groups, Some("org"), Some(json!({"name": ""})))
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // Изменение: список отправителей, снятие скрытия и адреса.
        let one = format!("{groups}/{id}");
        let patch = json!({"senders": {"only": {"accounts": [alice], "groups": []}}, "hidden_from_gal": false});
        let (s, _, g) = env.call("PATCH", &one, Some("org"), Some(patch)).await;
        assert_eq!(s, StatusCode::OK, "{g}");
        assert_eq!(
            g["senders"],
            json!({"only": {"accounts": [alice], "groups": []}})
        );
        assert_eq!(g["hidden_from_gal"], false);
        assert_eq!(g["name"], "Staff");
        let (_, _, g) = env
            .call("PATCH", &one, Some("org"), Some(json!({"address": null})))
            .await;
        assert_eq!(g["address"], Value::Null);
        let (s, _, list) = env.call("GET", &groups, Some("operator"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(list.as_array().expect("list").len(), 2);
        let (s, _, _) = env
            .call(
                "PATCH",
                &one,
                Some("operator"),
                Some(json!({"hidden_from_gal": true})),
            )
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "оператор — только чтение");

        // Группа каталога: имя и участники — из каталога, ограничения — через API; удалить нельзя.
        let dir = DistributionGroup {
            id: GroupId::new(),
            tenant: TenantId::DEFAULT,
            name: "devs".into(),
            address: Some("devs@example.org".into()),
            external_id: Some("guid-1".into()),
            members: vec![],
            senders: SenderRestriction::Anyone,
            moderators: vec![],
            hidden_from_gal: false,
        };
        env.groups.put(dir.clone()).await.expect("put");
        let dir_url = format!("{groups}/{}", dir.id);
        let (s, _, _) = env
            .call(
                "PATCH",
                &dir_url,
                Some("org"),
                Some(json!({"name": "Devs"})),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, g) = env
            .call(
                "PATCH",
                &dir_url,
                Some("org"),
                Some(json!({"hidden_from_gal": true, "moderators": [alice]})),
            )
            .await;
        assert_eq!((s, &g["hidden_from_gal"]), (StatusCode::OK, &json!(true)));
        let (s, _, _) = env.call("DELETE", &dir_url, Some("org"), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // Удаление группы: письма очереди отклоняются с уведомлением.
        let h = held(stored.id, "x@ext.example");
        env.groups
            .hold(TenantId::DEFAULT, h.clone())
            .await
            .expect("hold");
        let (s, _, _) = env.call("DELETE", &one, Some("org"), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert_eq!(
            env.moderation.decisions(),
            [Decision::Reject(h.id, "the group was deleted".into())]
        );
        let (s, _, _) = env.call("GET", &one, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let other = format!("/api/v1/orgs/{}/groups", env.other_tenant);
        let (s, _, _) = env.call("GET", &other, Some("org"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    /// Приёмка ADM-07: очередь модерации — одобрить, отклонить с уведомлением, сбой доставки.
    #[tokio::test]
    async fn moderation_queue() {
        let env = Env::new();
        let bob = env.account("bob@example.org").await.account;
        let (_, _, g) = env
            .call(
                "POST",
                &format!("{}/groups", base()),
                Some("org"),
                Some(json!({"name": "Staff", "address": "staff@example.org", "members": [{"account": bob}], "moderators": [bob]})),
            )
            .await;
        let gid: GroupId = g["id"].as_str().expect("id").parse().expect("uuid");
        let (h1, h2, h3) = (held(gid, "a@ext"), held(gid, "b@ext"), held(gid, "c@ext"));
        for h in [&h1, &h2, &h3] {
            env.groups
                .hold(TenantId::DEFAULT, h.clone())
                .await
                .expect("hold");
        }
        let queue = format!("{}/moderation?group={gid}", base());
        let (s, _, list) = env.call("GET", &queue, Some("operator"), None).await;
        assert_eq!(s, StatusCode::OK);
        let senders: Vec<_> = list
            .as_array()
            .expect("list")
            .iter()
            .map(|h| h["sender"].clone())
            .collect();
        assert_eq!(senders, [json!("a@ext"), json!("b@ext"), json!("c@ext")]);
        assert_eq!(list[0]["message_id"], "m1@ext");

        let url = |id: HeldId, what: &str| format!("{}/moderation/{id}/{what}", base());
        let (s, _, _) = env
            .call("POST", &url(h1.id, "approve"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _, _) = env
            .call("POST", &url(h1.id, "approve"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "уже решено");
        let (s, _, _) = env
            .call(
                "POST",
                &url(h2.id, "reject"),
                Some("org"),
                Some(json!({"reason": "off-topic"})),
            )
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _, _) = env
            .call(
                "POST",
                &url(h3.id, "reject"),
                Some("org"),
                Some(json!({"reason": "two\nlines"})),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, _) = env
            .call("POST", &url(h3.id, "approve"), Some("operator"), None)
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // Сбой доставки: письмо возвращается в очередь.
        env.moderation.fail_next(Error::UNAVAILABLE);
        let (s, _, _) = env
            .call("POST", &url(h3.id, "approve"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        let (_, _, list) = env.call("GET", &queue, Some("org"), None).await;
        assert_eq!(list.as_array().expect("list").len(), 1);
        let (s, _, _) = env
            .call("POST", &url(h3.id, "reject"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert_eq!(
            env.moderation.decisions(),
            [
                Decision::Deliver(h1.id, Some(gid)),
                Decision::Reject(h2.id, "off-topic".into()),
                Decision::Reject(h3.id, super::DEFAULT_REASON.into()),
            ]
        );
        let (_, _, list) = env.call("GET", &queue, Some("org"), None).await;
        assert!(list.as_array().expect("list").is_empty());
    }

    /// Приёмка ADM-20, ADM-32: имена участников, отправителей и модераторов — в том же ответе, одним запросом
    /// `AccountStore::by_ids` на группу из 500 участников.
    #[tokio::test]
    async fn names_in_one_response() {
        let cfg = AdminConfig {
            page_size: 10,
            max_page_size: 64,
            ..config()
        };
        let env = Env::with(cfg, Vec::new());
        let mut ids = Vec::new();
        for i in 0..500 {
            let login = format!("User{i:03}@example.org");
            ids.push((env.account(&login).await.account, login));
        }
        let outsider = env.account("zed@example.org").await.account;
        let groups = format!("{}/groups", base());
        let (_, _, inner) = env
            .call(
                "POST",
                &groups,
                Some("org"),
                Some(json!({"name": "Внутренняя"})),
            )
            .await;
        let inner_id = inner["id"].as_str().expect("id").to_owned();
        let mut members: Vec<Value> = ids.iter().map(|(id, _)| json!({ "account": id })).collect();
        members.push(json!({ "group": inner_id }));
        let body = json!({
            "name": "Все", "members": members, "moderators": [ids[0].0],
            "senders": {"only": {"accounts": [outsider], "groups": [inner_id]}}
        });
        env.account_calls.take();
        let (s, _, g) = env.call("POST", &groups, Some("org"), Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{g}");
        let id = g["id"].as_str().expect("id").to_owned();
        let check = |g: &Value| {
            let names = g["names"].as_object().expect("names");
            assert_eq!(names.len(), 502, "500 участников, отправитель, группа");
            for (id, login) in &ids {
                assert_eq!(names[&id.to_string()], json!(login));
            }
            assert_eq!(names[&outsider.to_string()], "zed@example.org");
            assert_eq!(names[&inner_id], "Внутренняя");
            assert_eq!(g["members"].as_array().expect("members").len(), 501);
            assert_eq!(
                g["members"][0],
                json!({ "account": ids[0].0 }),
                "вид участника прежний"
            );
        };
        check(&g);
        let one = format!("{groups}/{id}");
        env.account_calls.take();
        let (s, _, g) = env.call("GET", &one, Some("operator"), None).await;
        assert_eq!(s, StatusCode::OK);
        check(&g);
        assert_eq!(
            env.account_calls.take(),
            ["by_ids"],
            "чтение группы — один запрос имён"
        );
        let (_, _, list) = env.call("GET", &groups, Some("org"), None).await;
        assert_eq!(
            env.account_calls.take(),
            ["by_ids"],
            "список групп — один запрос имён"
        );
        let list = list.as_array().expect("list");
        check(list.iter().find(|g| g["id"] == id.as_str()).expect("group"));
        let inner = list
            .iter()
            .find(|g| g["id"] == inner_id.as_str())
            .expect("inner");
        assert_eq!(inner["names"], json!({}));

        // Изменение: ответ — с именами нового состава; удалённой группы в словаре нет.
        let patch =
            json!({"members": [{"account": outsider}], "senders": "internal", "moderators": []});
        let (s, _, g) = env.call("PATCH", &one, Some("org"), Some(patch)).await;
        assert_eq!(s, StatusCode::OK, "{g}");
        assert_eq!(
            g["names"],
            json!({ outsider.to_string(): "zed@example.org" })
        );
        let dir = DistributionGroup {
            id: GroupId::new(),
            tenant: TenantId::DEFAULT,
            name: "devs".into(),
            address: None,
            external_id: Some("guid-1".into()),
            members: vec![
                GroupMember::Group(GroupId::new()),
                GroupMember::Account(AccountId::new()),
            ],
            senders: SenderRestriction::Anyone,
            moderators: vec![],
            hidden_from_gal: false,
        };
        env.groups.put(dir.clone()).await.expect("put");
        let (s, _, g) = env
            .call("GET", &format!("{groups}/{}", dir.id), Some("org"), None)
            .await;
        assert_eq!((s, &g["names"]), (StatusCode::OK, &json!({})));
    }

    /// Приёмка ADM-20: `members` и `senders` в схеме — не произвольный объект, и схема совпадает с контрактом.
    #[tokio::test]
    async fn precise_schemas() {
        let env = Env::new();
        let (_, _, spec) = env.call("GET", "/api/openapi.json", None, None).await;
        let schemas = &spec["components"]["schemas"];
        let member = "#/components/schemas/GroupMember";
        let senders = "#/components/schemas/SenderRestriction";
        for (name, required) in [
            ("GroupInfo", true),
            ("NewGroup", false),
            ("GroupPatch", false),
        ] {
            let props = &schemas[name]["properties"];
            let text = props.to_string();
            assert!(
                text.contains(member) && text.contains(senders),
                "{name}: {text}"
            );
            if required {
                assert_eq!(props["members"]["items"]["$ref"], member);
                assert_eq!(props["senders"]["$ref"], senders);
                assert_eq!(
                    props["names"]["additionalProperties"]["type"], "string",
                    "{props}"
                );
            }
        }
        let variants = schemas["GroupMember"]["oneOf"].as_array().expect("oneOf");
        let keys: Vec<_> = variants
            .iter()
            .map(|v| {
                assert_eq!(v["type"], "object");
                let key = v["required"][0].as_str().expect("required");
                assert_eq!(v["properties"][key]["type"], "string");
                key
            })
            .collect();
        assert_eq!(keys, ["account", "group"]);
        let variants = schemas["SenderRestriction"]["oneOf"]
            .as_array()
            .expect("oneOf");
        let text = schemas["SenderRestriction"].to_string();
        assert!(
            text.contains("\"anyone\"") && text.contains("\"internal\""),
            "{text}"
        );
        let only = variants
            .iter()
            .find(|v| v["properties"]["only"].is_object())
            .expect("only");
        let fields = &only["properties"]["only"];
        assert_eq!(fields["required"], json!(["accounts", "groups"]));
        assert_eq!(fields["properties"]["accounts"]["items"]["type"], "string");

        // Вид контракта в `serde` — тот же, что описывает схема.
        let (a, g) = (AccountId::new(), GroupId::new());
        assert_eq!(
            json!([GroupMember::Account(a), GroupMember::Group(g)]),
            json!([{ "account": a }, { "group": g }])
        );
        assert_eq!(
            json!([SenderRestriction::Anyone, SenderRestriction::Internal]),
            json!(["anyone", "internal"])
        );
        let only = SenderRestriction::Only {
            accounts: vec![a],
            groups: vec![g],
        };
        assert_eq!(
            json!(only),
            json!({"only": {"accounts": [a], "groups": [g]}})
        );
    }

    /// Приёмка ADM-19: тема письма очереди — только по `include=subject`, администратору, с записью в аудит.
    #[tokio::test]
    async fn held_subjects_on_request() {
        use tp_contracts::blob::BlobStore;

        let env = Env::new();
        let (_, _, g) = env
            .call(
                "POST",
                &format!("{}/groups", base()),
                Some("org"),
                Some(json!({"name": "Staff", "address": "staff@example.org"})),
            )
            .await;
        let gid: GroupId = g["id"].as_str().expect("id").parse().expect("uuid");
        let h1 = held(gid, "a@ext");
        let h2 = HeldMessage {
            blob: BlobKey {
                hash: BlobHash([2; 32]),
                ..h1.blob
            },
            ..held(gid, "b@ext")
        };
        let raw = "From: A <a@ext>\r\nSubject: =?UTF-8?B?0J/RgNC40LLQtdGC?=\r\n\r\nтело\r\n";
        env.held_blobs.put(&h1.blob, raw.into()).await.expect("put");
        for h in [&h1, &h2] {
            env.groups
                .hold(TenantId::DEFAULT, h.clone())
                .await
                .expect("hold");
        }
        let content = |env: &Env| -> Vec<_> {
            let log = env.audit.records();
            log.into_iter()
                .filter(|r| r.content_messages.is_some())
                .collect()
        };
        let queue = format!("{}/moderation", base());
        let (s, _, list) = env.call("GET", &queue, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(!list[0].as_object().expect("item").contains_key("subject"));
        assert!(content(&env).is_empty());

        let with = format!("{queue}?include=subject");
        let (s, _, list) = env.call("GET", &with, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK, "{list}");
        assert_eq!(list[0]["subject"], "Привет");
        assert_eq!(list[0]["sender"], "a@ext");
        assert_eq!(list[1]["subject"], Value::Null, "объекта письма нет");
        let log = content(&env);
        assert_eq!(log.len(), 1);
        assert_eq!(
            (
                log[0].route.as_str(),
                log[0].status,
                log[0].content_messages
            ),
            ("/api/v1/orgs/{org}/moderation", 200, Some(2))
        );

        let (s, _, _) = env.call("GET", &with, Some("operator"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(content(&env).last().expect("audit").status, 403);
        let bad = format!("{queue}?include=all");
        let (s, _, _) = env.call("GET", &bad, Some("org"), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    /// CTR-43: письмо правила потока в очереди — источник и правило видны, одобрение — доставка получателям.
    #[tokio::test]
    async fn flow_rule_held() {
        let env = Env::new();
        let moderator = AccountId::new();
        let h = HeldMessage {
            group: None,
            source: HoldSource::FlowRule {
                rule: "Внешние вложения".into(),
            },
            moderators: vec![Moderator::Account(moderator)],
            recipients: vec!["boss@example.org".into()],
            ..held(GroupId::new(), "x@ext.example")
        };
        env.groups
            .hold(TenantId::DEFAULT, h.clone())
            .await
            .expect("hold");
        let queue = format!("{}/moderation", base());
        let (s, _, list) = env.call("GET", &queue, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK);
        let item = &list[0];
        assert_eq!(
            (&item["group"], &item["source"], &item["rule"]),
            (
                &json!(null),
                &json!("flow_rule"),
                &json!("Внешние вложения")
            )
        );
        assert_eq!(item["moderators"], json!([format!("account:{moderator}")]));
        assert_eq!(item["recipients"], json!(["boss@example.org"]));
        let url = format!("{queue}/{}/approve", h.id);
        let (s, _, _) = env.call("POST", &url, Some("org"), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert_eq!(env.moderation.decisions(), [Decision::Deliver(h.id, None)]);
    }
}
