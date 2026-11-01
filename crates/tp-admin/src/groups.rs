//! Группы рассылки: настройки и модерация (SRS 4.5; CONTRACTS.md#Groups).
//!
//! - группы организации (`/api/v1/orgs/{org}/groups`): список, создание, чтение, изменение (`PATCH`), удаление;
//!   ограничение круга отправителей (`anyone`, `internal`, `{"only": {"accounts": [...], "groups": [...]}}`),
//!   модераторы (непусто — группа модерируемая), скрытие из GAL. У групп каталога (`external_id`, AUTH-07)
//!   имя, адрес и участники — из каталога: через API меняются только ограничения, модераторы и скрытие
//!   (если они не сопоставлены в секции каталога, иначе синхронизация их перезапишет); удалить группу каталога
//!   нельзя;
//! - очередь модерации (`/api/v1/orgs/{org}/moderation`): письма по времени постановки, одобрение — доставка
//!   участникам, отклонение — уведомление отправителя с причиной. Письмо забирается из очереди
//!   (`GroupStore::take_held`) и передаётся [`Moderation`]; ошибка доставки возвращает его в очередь.
//!
//! Доставки одобренного письма и уведомления об отказе в контрактах нет: [`Moderation`] — временный трейт полосы
//! (реализация — `tp-smtp`: раскрытие группы, приём в ящики, DSN, журнал доставки).

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;
use tp_contracts::groups::{DistributionGroup, GroupMember, HeldMessage, SenderRestriction};
use tp_contracts::{AccountId, Error, GroupId, HeldId, Result, TenantId};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::server::{Admin, ApiResult, Problem};

/// Решения модерации (временный трейт полосы до контракта).
#[async_trait]
pub trait Moderation: Send + Sync + 'static {
    /// Одобрено: доставить письмо участникам `group` (раскрытие с вложенными, приём в ящики, журнал доставки)
    /// и освободить ссылку на объект письма.
    async fn deliver(
        &self,
        tenant: TenantId,
        group: &DistributionGroup,
        message: &HeldMessage,
    ) -> Result<()>;

    /// Отклонено: уведомить отправителя (DSN, RFC 3464) с причиной `reason`, освободить ссылку на объект письма.
    async fn reject(&self, tenant: TenantId, message: &HeldMessage, reason: &str) -> Result<()>;
}

/// Группа рассылки.
#[derive(Debug, Serialize, ToSchema)]
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
    #[schema(value_type = Vec<Object>)]
    pub members: Vec<GroupMember>,
    /// Кто может писать: `"anyone"`, `"internal"`, `{"only": {"accounts": [...], "groups": [...]}}`.
    #[schema(value_type = Object)]
    pub senders: SenderRestriction,
    /// Модераторы; непусто — письма не от модераторов ждут одобрения.
    #[schema(value_type = Vec<String>)]
    pub moderators: Vec<AccountId>,
    /// Скрыта из адресной книги.
    pub hidden_from_gal: bool,
}

impl From<DistributionGroup> for GroupInfo {
    fn from(g: DistributionGroup) -> Self {
        Self {
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
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewGroup {
    /// Отображаемое имя.
    pub name: String,
    /// Адрес рассылки.
    #[serde(default)]
    pub address: Option<String>,
    /// Участники.
    #[serde(default)]
    #[schema(value_type = Vec<Object>)]
    pub members: Vec<GroupMember>,
    /// Ограничение отправителей (по умолчанию — без ограничений).
    #[serde(default)]
    #[schema(value_type = Object)]
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
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GroupPatch {
    /// Отображаемое имя (не для групп каталога).
    pub name: Option<String>,
    /// Адрес рассылки; `null` — снять (не для групп каталога).
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub address: Option<Option<String>>,
    /// Участники (не для групп каталога).
    #[schema(value_type = Option<Vec<Object>>)]
    pub members: Option<Vec<GroupMember>>,
    /// Ограничение отправителей.
    #[schema(value_type = Option<Object>)]
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
    /// Группа-адресат.
    pub group: String,
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
}

impl From<HeldMessage> for HeldInfo {
    fn from(h: HeldMessage) -> Self {
        Self {
            id: h.id.to_string(),
            group: h.group.to_string(),
            sender: h.sender,
            sender_account: h.sender_account.map(|a| a.to_string()),
            message_id: h.message_id,
            size: h.size,
            held_at: h.held_at,
        }
    }
}

/// Очередь модерации: фильтр и размер.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct HeldQuery {
    /// Только письма группы (UUID).
    #[param(value_type = Option<String>)]
    pub group: Option<GroupId>,
    /// Размер (по умолчанию и наибольший — из конфигурации).
    pub limit: Option<usize>,
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
    components(schemas(GroupInfo, NewGroup, GroupPatch, HeldInfo, Rejection))
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
    accounts.sort_unstable();
    accounts.dedup();
    groups.sort_unstable();
    groups.dedup();
    for x in accounts {
        if a.d.accounts.get(g.tenant, x).await?.is_none() {
            return Err(Error::invalid(format!("unknown account {x}")));
        }
    }
    for x in groups {
        if x != g.id && a.d.groups.get(g.tenant, x).await?.is_none() {
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
    Ok(Json(list.into_iter().map(Into::into).collect()))
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
    Ok((StatusCode::CREATED, Json(g.into())))
}

/// Группа.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/groups/{group}", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)"), ("group" = String, Path, description = "Группа (UUID)")),
    responses((status = 200, body = GroupInfo), (status = 404, body = Problem)))]
async fn get_group(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, GroupId)>,
) -> ApiResult<Json<GroupInfo>> {
    Ok(Json(group(&a, org, id).await?.into()))
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
    Ok(Json(g.into()))
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

/// Очередь модерации организации или группы.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/moderation", tag = "groups",
    params(("org" = String, Path, description = "Организация (UUID)"), HeldQuery),
    responses((status = 200, body = Vec<HeldInfo>), (status = 403, body = Problem)))]
async fn list_held(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Query(q): Query<HeldQuery>,
) -> ApiResult<Json<Vec<HeldInfo>>> {
    let held = a.d.groups.held(org, q.group, a.cfg.page(q.limit)).await?;
    Ok(Json(held.into_iter().map(Into::into).collect()))
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
    let g = match a.d.groups.get(org, h.group).await {
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
    if let Err(e) = a.d.moderation.deliver(org, &g, &h).await {
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
    if reason.is_empty() || reason.chars().count() > 500 || reason.chars().any(char::is_control) {
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
    use tp_contracts::groups::{DistributionGroup, GroupStore, HeldMessage, SenderRestriction};
    use tp_contracts::{BlobHash, BlobKey, Error, Gen, GroupId, HeldId, TenantId};

    use crate::testenv::{Decision, Env};

    fn base() -> String {
        format!("/api/v1/orgs/{}", TenantId::DEFAULT)
    }

    fn held(group: GroupId, sender: &str) -> HeldMessage {
        HeldMessage {
            id: HeldId::new(),
            group,
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
                Decision::Deliver(h1.id, gid),
                Decision::Reject(h2.id, "off-topic".into()),
                Decision::Reject(h3.id, super::DEFAULT_REASON.into()),
            ]
        );
        let (_, _, list) = env.call("GET", &queue, Some("org"), None).await;
        assert!(list.as_array().expect("list").is_empty());
    }
}
