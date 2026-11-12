//! Ящики сверх учётных записей (SRS 4.7, 5.2; решение 2026-11-04-exchange-parity; контракты CTR-35):
//!
//! - списки надёжных и заблокированных отправителей ящика (`…/accounts/{account}/senders`): чтение и замена целиком
//!   (записи приводятся к хранимому виду `SenderList::normalized`);
//! - отключённые ящики (`/api/v1/orgs/{org}/disabled-accounts`): список со сроком хранения, восстановление,
//!   подключение к другой учётной записи (`AccountStore::attach_account`, режим содержимого её ящика —
//!   `existing`: `keep` или `delete`);
//! - остановка синхронизации каталога при удалении больше порога учётных записей (`/api/v1/orgs/{org}/sync-hold`):
//!   чтение и подтверждение администратором (`SyncGate::approve_sync`) — следующая синхронизация выполняет отключение;
//! - аудит доступа к ящикам: политика организации (`/api/v1/orgs/{org}/mailbox-audit/policy`) и ящика
//!   (`…/accounts/{account}/audit-policy`, чтение — действующая), выборка журнала (`/api/v1/orgs/{org}/mailbox-audit`;
//!   доступна аудитору, недоступна оператору).
//!
//! Хранилища списков, аудита и остановки — [`crate::server::ParityStores`] (не подключены — 503).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tp_contracts::audit::{
    AuditAction, AuditActorKind, AuditCursor, AuditEntry, AuditPolicy, AuditQuery,
};
use tp_contracts::auth::MassRemoval;
use tp_contracts::meta::{AccountScope, DisabledAccount, ExistingContent};
use tp_contracts::senders::SenderList;
use tp_contracts::{AccountId, TenantId};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::accounts::{AccountInfo, account, account_info};
use crate::directory::stores;
use crate::server::{Admin, ApiResult, Problem};

/// Маршрут выборки журнала аудита ящиков (доступен аудитору).
pub const MAILBOX_AUDIT_ROUTE: &str = "/api/v1/orgs/{org}/mailbox-audit";

/// Списки отправителей ящика.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SenderListBody {
    /// Надёжные адреса и домены (`user@example.com`, `example.com`).
    #[serde(default)]
    pub safe: Vec<String>,
    /// Заблокированные адреса и домены.
    #[serde(default)]
    pub blocked: Vec<String>,
}

impl From<SenderList> for SenderListBody {
    fn from(l: SenderList) -> Self {
        Self {
            safe: l.safe,
            blocked: l.blocked,
        }
    }
}

/// Отключённый ящик.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DisabledInfo {
    /// Учётная запись.
    pub id: String,
    /// Имя входа.
    pub login: String,
    /// Отображаемое имя.
    pub display_name: String,
    /// Идентификатор в каталоге.
    pub external_id: Option<String>,
    /// Срок хранения: после него ящик удаляется окончательно.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub until: OffsetDateTime,
}

impl From<DisabledAccount> for DisabledInfo {
    fn from(d: DisabledAccount) -> Self {
        Self {
            id: d.account.id.to_string(),
            login: d.account.login,
            display_name: d.account.display_name,
            external_id: d.account.external_id,
            until: d.until,
        }
    }
}

/// Содержимое ящика учётной записи, к которой подключается отключённый ящик.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExistingMode {
    /// Перенести письма в подключаемый ящик.
    Keep,
    /// Перенести письма в корзину подключаемого ящика.
    Delete,
}

impl From<ExistingMode> for ExistingContent {
    fn from(m: ExistingMode) -> Self {
        match m {
            ExistingMode::Keep => ExistingContent::Keep,
            ExistingMode::Delete => ExistingContent::Delete,
        }
    }
}

/// Подключение отключённого ящика к учётной записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachBody {
    /// Учётная запись, к которой подключается ящик (её запись удаляется, вход и адрес переходят к ящику).
    #[schema(value_type = String)]
    pub to: AccountId,
    /// Что сделать с письмами её ящика; нет — ящик должен быть пуст (иначе 400).
    #[serde(default)]
    pub existing: Option<ExistingMode>,
}

/// Синхронизация каталога, остановленная массовым удалением учётных записей.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SyncHold {
    /// Учётных записей к отключению.
    pub removed: u64,
    /// Учётных записей в организации.
    pub total: u64,
    /// Обнаружено.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub detected_at: OffsetDateTime,
    /// Подтверждено: следующая синхронизация выполнит отключение.
    pub approved: bool,
}

impl From<MassRemoval> for SyncHold {
    fn from(m: MassRemoval) -> Self {
        Self {
            removed: m.removed,
            total: m.total,
            detected_at: m.detected_at,
            approved: m.approved,
        }
    }
}

/// Политика аудита ящика.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditPolicyBody {
    /// Записывать действия не владельца (представители, администраторы).
    pub non_owner: bool,
    /// Записываемые действия владельца: `delete`, `move`, `send`, `change_rules`, `change_forwarding`.
    #[schema(value_type = Vec<String>)]
    pub owner: Vec<AuditAction>,
}

impl From<AuditPolicy> for AuditPolicyBody {
    fn from(p: AuditPolicy) -> Self {
        Self {
            non_owner: p.non_owner,
            owner: p.owner.into_iter().collect(),
        }
    }
}

impl From<AuditPolicyBody> for AuditPolicy {
    fn from(b: AuditPolicyBody) -> Self {
        Self {
            non_owner: b.non_owner,
            owner: b.owner.into_iter().collect(),
        }
    }
}

/// Условия выборки журнала аудита ящиков (по «и»).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct MailboxAuditQuery {
    /// Ящик (UUID учётной записи).
    #[serde(default)]
    #[param(value_type = Option<String>)]
    pub account: Option<AccountId>,
    /// Исполнитель (имя входа или субъект администратора).
    #[serde(default)]
    pub actor: Option<String>,
    /// Кто относительно ящика: `owner`, `delegate`, `admin`.
    #[serde(default)]
    #[param(value_type = Option<String>)]
    pub actor_kind: Option<AuditActorKind>,
    /// Действие (`login`, `read`, `move`, `delete`, `send`, `send_as`, `send_on_behalf`, `change_rights`,
    /// `change_rules`, `change_forwarding`).
    #[serde(default)]
    #[param(value_type = Option<String>)]
    pub action: Option<AuditAction>,
    /// Начало интервала (RFC 3339), включительно.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub since: Option<OffsetDateTime>,
    /// Конец интервала (RFC 3339), не включительно.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub until: Option<OffsetDateTime>,
    /// Размер страницы (по умолчанию и наибольший — `admin.page_size`, `admin.max_page_size`).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Продолжение — `next` предыдущей страницы.
    #[serde(default)]
    pub after: Option<String>,
}

/// Запись аудита ящика.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MailboxAuditEntry {
    /// Ящик (учётная запись).
    pub account: String,
    /// Момент действия.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub at: OffsetDateTime,
    /// Исполнитель.
    pub actor: String,
    /// Учётная запись исполнителя; `null` — администратор.
    pub actor_account: Option<String>,
    /// Кто относительно ящика.
    #[schema(value_type = String)]
    pub actor_kind: AuditActorKind,
    /// Действие.
    #[schema(value_type = String)]
    pub action: AuditAction,
    /// Папка.
    pub mailbox: Option<String>,
    /// Письмо.
    pub message: Option<String>,
    /// Пояснение (Message-ID, адрес, право).
    pub detail: Option<String>,
    /// Клиент: протокол и адрес.
    pub client: Option<String>,
}

impl From<AuditEntry> for MailboxAuditEntry {
    fn from(e: AuditEntry) -> Self {
        Self {
            account: e.scope.account.to_string(),
            at: e.at,
            actor: e.actor,
            actor_account: e.actor_account.map(|x| x.to_string()),
            actor_kind: e.actor_kind,
            action: e.action,
            mailbox: e.object.mailbox.map(|x| x.to_string()),
            message: e.object.message.map(|x| x.to_string()),
            detail: e.object.detail,
            client: e.client,
        }
    }
}

/// Страница журнала аудита ящиков.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MailboxAuditPage {
    /// Записи по возрастанию времени.
    pub entries: Vec<MailboxAuditEntry>,
    /// Продолжение (`after` следующего запроса); `null` — записей больше нет.
    pub next: Option<String>,
}

/// Маршруты ящиков.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route(
        "/api/v1/orgs/{org}/accounts/{account}/senders",
        get(get_senders).put(put_senders),
    )
    .route("/api/v1/orgs/{org}/disabled-accounts", get(list_disabled))
    .route(
        "/api/v1/orgs/{org}/disabled-accounts/{account}/restore",
        post(restore_disabled),
    )
    .route(
        "/api/v1/orgs/{org}/disabled-accounts/{account}/attach",
        post(attach_disabled),
    )
    .route("/api/v1/orgs/{org}/sync-hold", get(get_sync_hold))
    .route("/api/v1/orgs/{org}/sync-hold/approve", post(approve_sync))
    .route(MAILBOX_AUDIT_ROUTE, get(search_audit))
    .route(
        "/api/v1/orgs/{org}/mailbox-audit/policy",
        put(put_org_audit_policy).delete(delete_org_audit_policy),
    )
    .route(
        "/api/v1/orgs/{org}/accounts/{account}/audit-policy",
        get(get_audit_policy)
            .put(put_audit_policy)
            .delete(delete_audit_policy),
    )
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(
        get_senders,
        put_senders,
        list_disabled,
        restore_disabled,
        attach_disabled,
        get_sync_hold,
        approve_sync,
        search_audit,
        put_org_audit_policy,
        delete_org_audit_policy,
        get_audit_policy,
        put_audit_policy,
        delete_audit_policy
    ),
    components(schemas(
        SenderListBody,
        DisabledInfo,
        ExistingMode,
        AttachBody,
        SyncHold,
        AuditPolicyBody,
        MailboxAuditEntry,
        MailboxAuditPage
    ))
)]
pub(crate) struct Doc;

async fn scope(a: &Admin, org: TenantId, id: AccountId) -> ApiResult<AccountScope> {
    account(a, org, id).await?;
    Ok(AccountScope {
        tenant: org,
        account: id,
    })
}

/// Списки отправителей ящика.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/senders", tag = "mailboxes",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    responses((status = 200, body = SenderListBody), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn get_senders(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<Json<SenderListBody>> {
    let s = stores(&a)?;
    let scope = scope(&a, org, id).await?;
    Ok(Json(s.senders.sender_list(scope).await?.into()))
}

/// Заменяет списки отправителей ящика целиком; ответ — хранимый вид.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/accounts/{account}/senders", tag = "mailboxes",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    request_body = SenderListBody,
    responses((status = 200, body = SenderListBody), (status = 400, body = Problem), (status = 404, body = Problem),
        (status = 503, body = Problem)))]
async fn put_senders(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
    Json(body): Json<SenderListBody>,
) -> ApiResult<Json<SenderListBody>> {
    let s = stores(&a)?;
    let scope = scope(&a, org, id).await?;
    let list = SenderList {
        safe: body.safe,
        blocked: body.blocked,
    }
    .normalized()?;
    s.senders.put_sender_list(scope, list.clone()).await?;
    Ok(Json(list.into()))
}

/// Отключённые ящики организации по сроку хранения.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/disabled-accounts", tag = "mailboxes",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = Vec<DisabledInfo>)))]
async fn list_disabled(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<Vec<DisabledInfo>>> {
    let list = a.d.accounts.disabled_accounts(org).await?;
    Ok(Json(list.into_iter().map(DisabledInfo::from).collect()))
}

/// Восстанавливает отключённый ящик (действующий — без изменений).
#[utoipa::path(post, path = "/api/v1/orgs/{org}/disabled-accounts/{account}/restore", tag = "mailboxes",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    responses((status = 204), (status = 404, body = Problem)))]
async fn restore_disabled(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<StatusCode> {
    a.d.accounts.restore_account(org, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Подключает отключённый ящик к учётной записи `to`: ящик получает её вход и адрес, её запись удаляется.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/disabled-accounts/{account}/attach", tag = "mailboxes",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Отключённый ящик (UUID)")),
    request_body = AttachBody,
    responses((status = 200, body = AccountInfo),
        (status = 400, description = "ящик действует или ящик `to` не пуст без `existing`", body = Problem),
        (status = 404, body = Problem), (status = 409, description = "учётная запись `to` отключена", body = Problem)))]
async fn attach_disabled(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
    Json(body): Json<AttachBody>,
) -> ApiResult<Json<AccountInfo>> {
    let acc =
        a.d.accounts
            .attach_account(org, id, body.to, body.existing.map(Into::into))
            .await?;
    Ok(Json(account_info(&a, acc).await?))
}

/// Остановленная синхронизация каталога; нет остановки — 404.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/sync-hold", tag = "mailboxes",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = SyncHold), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn get_sync_hold(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<SyncHold>> {
    let hold = stores(&a)?
        .sync
        .pending_sync(org)
        .await?
        .ok_or(tp_contracts::Error::NotFound)?;
    Ok(Json(hold.into()))
}

/// Подтверждает отключение учётных записей сверх порога: следующая синхронизация его выполнит.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/sync-hold/approve", tag = "mailboxes",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = SyncHold), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn approve_sync(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<SyncHold>> {
    Ok(Json(stores(&a)?.sync.approve_sync(org).await?.into()))
}

/// Выборка журнала аудита доступа к ящикам организации.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/mailbox-audit", tag = "audit",
    params(("org" = String, Path, description = "Организация (UUID)"), MailboxAuditQuery),
    responses((status = 200, body = MailboxAuditPage), (status = 400, body = Problem),
        (status = 503, body = Problem)))]
async fn search_audit(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Query(q): Query<MailboxAuditQuery>,
) -> ApiResult<Json<MailboxAuditPage>> {
    let s = stores(&a)?;
    let query = AuditQuery {
        account: q.account,
        actor: q.actor,
        actor_kind: q.actor_kind,
        action: q.action,
        since: q.since,
        until: q.until,
        limit: a.cfg.page(q.limit),
        after: q.after.map(AuditCursor),
    };
    query.validate()?;
    let page = s.mailbox_audit.search_audit(org, &query).await?;
    Ok(Json(MailboxAuditPage {
        entries: page
            .entries
            .into_iter()
            .map(MailboxAuditEntry::from)
            .collect(),
        next: page.next.map(|c| c.0),
    }))
}

/// Задаёт политику аудита ящиков организации.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/mailbox-audit/policy", tag = "audit",
    params(("org" = String, Path, description = "Организация (UUID)")),
    request_body = AuditPolicyBody,
    responses((status = 200, body = AuditPolicyBody), (status = 400, body = Problem), (status = 503, body = Problem)))]
async fn put_org_audit_policy(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Json(body): Json<AuditPolicyBody>,
) -> ApiResult<Json<AuditPolicyBody>> {
    let p = AuditPolicy::from(body);
    p.validate()?;
    stores(&a)?
        .mailbox_audit
        .set_audit_policy(org, None, Some(p.clone()))
        .await?;
    Ok(Json(p.into()))
}

/// Сбрасывает политику аудита организации: действует политика по умолчанию (только действия не владельца).
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/mailbox-audit/policy", tag = "audit",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 204), (status = 503, body = Problem)))]
async fn delete_org_audit_policy(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<StatusCode> {
    stores(&a)?
        .mailbox_audit
        .set_audit_policy(org, None, None)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Действующая политика аудита ящика (ящика, иначе организации, иначе по умолчанию).
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/audit-policy", tag = "audit",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    responses((status = 200, body = AuditPolicyBody), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn get_audit_policy(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<Json<AuditPolicyBody>> {
    let s = stores(&a)?;
    let scope = scope(&a, org, id).await?;
    Ok(Json(s.mailbox_audit.audit_policy(scope).await?.into()))
}

/// Задаёт политику аудита ящика (приоритетнее политики организации).
#[utoipa::path(put, path = "/api/v1/orgs/{org}/accounts/{account}/audit-policy", tag = "audit",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    request_body = AuditPolicyBody,
    responses((status = 200, body = AuditPolicyBody), (status = 400, body = Problem), (status = 404, body = Problem),
        (status = 503, body = Problem)))]
async fn put_audit_policy(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
    Json(body): Json<AuditPolicyBody>,
) -> ApiResult<Json<AuditPolicyBody>> {
    let s = stores(&a)?;
    scope(&a, org, id).await?;
    let p = AuditPolicy::from(body);
    p.validate()?;
    s.mailbox_audit
        .set_audit_policy(org, Some(id), Some(p.clone()))
        .await?;
    Ok(Json(p.into()))
}

/// Сбрасывает политику аудита ящика: действует политика организации.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/accounts/{account}/audit-policy", tag = "audit",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    responses((status = 204), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn delete_audit_policy(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<StatusCode> {
    let s = stores(&a)?;
    scope(&a, org, id).await?;
    s.mailbox_audit
        .set_audit_policy(org, Some(id), None)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;
    use time::{Duration, OffsetDateTime};
    use tp_contracts::TenantId;
    use tp_contracts::audit::{AuditAction, AuditActorKind, AuditEntry, AuditObject, MailboxAudit};
    use tp_contracts::auth::SyncGate;
    use tp_contracts::meta::AccountStore;

    use crate::testenv::Env;

    fn org() -> String {
        format!("/api/v1/orgs/{}", TenantId::DEFAULT)
    }

    #[tokio::test]
    async fn sender_lists() {
        let env = Env::new();
        let acc = env.account("user@example.org").await;
        let path = format!("{}/accounts/{}/senders", org(), acc.account);
        let (s, _, b) = env.call("GET", &path, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, json!({"safe": [], "blocked": []}));
        let body =
            json!({"safe": ["@Partner.example", "boss@corp.example"], "blocked": ["spam.example"]});
        let (s, _, b) = env.call("PUT", &path, Some("org"), Some(body)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["safe"], json!(["boss@corp.example", "partner.example"]));
        let (s, _, _) = env
            .call("PUT", &path, Some("org"), Some(json!({"safe": ["a b"]})))
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, _) = env
            .call("PUT", &path, Some("operator"), Some(json!({})))
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn disabled_restore_and_attach() {
        let env = Env::new();
        let old = env.account("old@example.org").await;
        let new = env.account("new@example.org").await;
        let until = OffsetDateTime::now_utc() + Duration::days(30);
        env.meta
            .disable_account(TenantId::DEFAULT, old.account, until)
            .await
            .expect("disable");
        let (s, _, list) = env
            .call(
                "GET",
                &format!("{}/disabled-accounts", org()),
                Some("operator"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(list[0]["login"], "old@example.org");
        let attach = format!("{}/disabled-accounts/{}/attach", org(), old.account);
        let (s, _, b) = env
            .call(
                "POST",
                &attach,
                Some("org"),
                Some(json!({"to": new.account, "existing": "keep"})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["id"], old.account.to_string());
        assert_eq!(b["login"], "new@example.org");

        let gone = env.account("gone@example.org").await;
        env.meta
            .disable_account(TenantId::DEFAULT, gone.account, until)
            .await
            .expect("disable");
        let (s, _, _) = env
            .call(
                "POST",
                &format!("{}/disabled-accounts/{}/restore", org(), gone.account),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, _, list) = env
            .call(
                "GET",
                &format!("{}/disabled-accounts", org()),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(list, json!([]));
    }

    #[tokio::test]
    async fn sync_hold_approve() {
        let env = Env::new();
        let path = format!("{}/sync-hold", org());
        let (s, _, _) = env.call("GET", &path, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        env.sync_gate
            .hold_sync(TenantId::DEFAULT, 10, 100, OffsetDateTime::now_utc())
            .await
            .expect("hold");
        let (_, _, b) = env.call("GET", &path, Some("operator"), None).await;
        assert_eq!(b["approved"], false);
        assert_eq!(b["removed"], 10);
        let (s, _, b) = env
            .call("POST", &format!("{path}/approve"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["approved"], true);
    }

    #[tokio::test]
    async fn audit_policy_and_search() {
        let env = Env::new();
        let acc = env.account("user@example.org").await;
        let policy = format!("{}/accounts/{}/audit-policy", org(), acc.account);
        let (_, _, b) = env.call("GET", &policy, Some("org"), None).await;
        assert_eq!(b, json!({"non_owner": true, "owner": []}));
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{}/mailbox-audit/policy", org()),
                Some("org"),
                Some(json!({"non_owner": true, "owner": ["read"]})),
            )
            .await;
        assert_eq!(
            s,
            StatusCode::BAD_REQUEST,
            "чтение владельцем не записывается"
        );
        let (s, _, _) = env
            .call(
                "PUT",
                &policy,
                Some("org"),
                Some(json!({"non_owner": true, "owner": ["delete", "send"]})),
            )
            .await;
        assert_eq!(s, StatusCode::OK);
        let (_, _, b) = env.call("GET", &policy, Some("org"), None).await;
        assert_eq!(b["owner"], json!(["delete", "send"]));

        let at = OffsetDateTime::now_utc();
        for action in [AuditAction::Delete, AuditAction::Read] {
            env.mailbox_audit
                .record_audit(AuditEntry {
                    scope: acc,
                    at,
                    actor: "user@example.org".into(),
                    actor_account: Some(acc.account),
                    actor_kind: AuditActorKind::Owner,
                    action,
                    object: AuditObject::default(),
                    client: Some("imap 192.0.2.1".into()),
                })
                .await
                .expect("record");
        }
        let search = format!("{}/mailbox-audit?account={}", org(), acc.account);
        let (s, _, page) = env.call("GET", &search, Some("auditor"), None).await;
        assert_eq!(s, StatusCode::OK, "{page}");
        assert_eq!(page["entries"].as_array().map(Vec::len), Some(1));
        assert_eq!(page["entries"][0]["action"], "delete");
        let (s, _, _) = env.call("GET", &search, Some("operator"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env
            .call(
                "GET",
                &format!("{}/mailbox-audit?action=nope", org()),
                Some("auditor"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, _) = env.call("DELETE", &policy, Some("org"), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
    }
}
