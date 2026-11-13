//! Корзина ящика (SRS 4.7, 5.2): список и восстановление элементов администратором.
//!
//! Список — `TrashStore::list` (новые первыми, страницы `offset`/`limit`) и действующий срок корзины ящика
//! (`trash_retention` цепочки организация → домен → ящик; `null` — значение по умолчанию конфигурации хранилища).
//! Срок задаётся политиками уровней ([`crate::accounts`]) и действует на элементы, удалённые после изменения.
//! Восстановление — `MailStore::restore`: письмо возвращается в исходную папку (нет — во «Входящие») с тем же
//! идентификатором, клиенты получают уведомление. Тема и отправитель писем (`subject`, `from`) — только по
//! `include=subject` и только администратору, с записью в аудит (`crate::content`); источник — `MailStore::fetch_raw`.
//! У скрытых писем (`…/trash/hidden`) тема и отправитель — из метаданных письма (`TrashStore::hidden_message`, MS-19).
//!
//! «Очистить корзину» в клиенте — настройка `trash.purge` (решение 2026-11-01-trash-purge): правило администратора
//! (`mode`, `user_can_choose`) — поле `trash_purge` политики уровня ([`crate::accounts`]), выбор пользователя
//! действует, только если правило его разрешает (`TrashPurgeSetting::effective`); настройка ящика —
//! `GET …/trash-purge`. Скрытые письма (`hide`) пользователь не видит, они хранятся до конца срока корзины:
//! список — `TrashStore::list_hidden`, восстановление администратором — `TrashStore::restore` (в исходную папку,
//! нет — во «Входящие»). Изменения записываются в журнал аудита каркасом ([`crate::server`]).

use std::sync::Arc;

use axum::extract::{MatchedPath, Path, Query, State};
use axum::http::Uri;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::meta::AccountScope;
use tp_contracts::retention::{
    Restored, TrashItem, TrashObject, TrashPurge, TrashPurgeRule, TrashPurgeSetting,
};
use tp_contracts::{AccountId, Error, MessageId, TenantId};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::accounts::account;
use crate::content::{self, Access};
use crate::server::{Admin, ApiResult, Caller, Problem};

/// Страница списка.
#[derive(Debug, Default, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct PageQuery {
    /// Смещение от начала списка.
    #[serde(default)]
    pub offset: usize,
    /// Размер страницы (по умолчанию и наибольший — из конфигурации).
    pub limit: Option<usize>,
    /// `subject` — отдать тему и отправителя писем (`subject`, `from`). Только администратору; запрос пишется в
    /// журнал аудита как доступ к содержимому; страница — не больше `admin.content_page_size`.
    pub include: Option<String>,
}

/// Элемент корзины.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TrashEntry {
    /// Письмо (идентификатор сохраняется при восстановлении).
    pub message: String,
    /// Исходная папка.
    pub origin: String,
    /// Размер, байт.
    pub size: u64,
    /// Момент приёма письма.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub received_at: OffsetDateTime,
    /// Момент удаления в корзину.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub deleted_at: OffsetDateTime,
    /// Окончательное удаление не раньше этого момента.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub expires_at: OffsetDateTime,
    /// Тема письма (заголовок `Subject`, декодированный, обрезанный). Поле есть только с `include=subject`;
    /// `null` — заголовка нет или письмо недоступно.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub subject: Option<Option<String>>,
    /// Отправитель (заголовок `From`: `Имя <адрес>`). Поле есть только с `include=subject`; `null` — как у темы.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub from: Option<Option<String>>,
}

/// Страница корзины.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TrashPage {
    /// Действующий срок корзины ящика; `null` — значение по умолчанию конфигурации.
    #[serde(with = "duration::option")]
    #[schema(value_type = Option<String>, example = "30d")]
    pub trash_retention: Option<std::time::Duration>,
    /// Элементы, новые первыми.
    pub items: Vec<TrashEntry>,
    /// Смещение следующей страницы; `null` — страница последняя.
    pub next_offset: Option<usize>,
}

/// Восстановленное письмо.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RestoredMessage {
    /// Письмо.
    pub message: String,
    /// Новый UID в папке.
    pub uid: u32,
    /// `ModSeq` изменения.
    pub modseq: u64,
}

/// Значение `trash.purge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PurgeMode {
    /// Скрыть: письмо исчезает у пользователя, хранится до конца срока корзины, восстанавливает администратор.
    #[default]
    Hide,
    /// Стереть сразу (под Legal Hold — как `hide`).
    Erase,
}

impl From<TrashPurge> for PurgeMode {
    fn from(m: TrashPurge) -> Self {
        match m {
            TrashPurge::Hide => Self::Hide,
            TrashPurge::Erase => Self::Erase,
        }
    }
}

impl From<PurgeMode> for TrashPurge {
    fn from(m: PurgeMode) -> Self {
        match m {
            PurgeMode::Hide => Self::Hide,
            PurgeMode::Erase => Self::Erase,
        }
    }
}

/// Правило администратора `trash.purge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TrashPurgeRuleBody {
    /// Значение администратора.
    pub mode: PurgeMode,
    /// Пользователь может выбрать своё значение; иначе действует `mode`.
    #[serde(default)]
    pub user_can_choose: bool,
}

impl From<TrashPurgeRule> for TrashPurgeRuleBody {
    fn from(r: TrashPurgeRule) -> Self {
        Self {
            mode: r.mode.into(),
            user_can_choose: r.user_can_choose,
        }
    }
}

impl From<TrashPurgeRuleBody> for TrashPurgeRule {
    fn from(r: TrashPurgeRuleBody) -> Self {
        Self {
            mode: r.mode.into(),
            user_can_choose: r.user_can_choose,
        }
    }
}

/// Настройка `trash.purge` ящика.
#[derive(Debug, Serialize, ToSchema)]
pub struct TrashPurgeInfo {
    /// Действующее правило администратора (цепочка организация → домен → ящик; не задано — `hide` без выбора).
    pub rule: TrashPurgeRuleBody,
    /// Выбор пользователя; `null` — не выбран. Хранится и без `user_can_choose`, но тогда не действует.
    pub user: Option<PurgeMode>,
    /// Действующее значение.
    pub effective: PurgeMode,
}

impl From<TrashPurgeSetting> for TrashPurgeInfo {
    fn from(s: TrashPurgeSetting) -> Self {
        Self {
            rule: s.rule.into(),
            user: s.user.map(PurgeMode::from),
            effective: s.effective().into(),
        }
    }
}

/// Маршруты корзины.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route(
        "/api/v1/orgs/{org}/accounts/{account}/trash",
        get(list_trash),
    )
    .route(
        "/api/v1/orgs/{org}/accounts/{account}/trash/{message}/restore",
        post(restore),
    )
    .route(
        "/api/v1/orgs/{org}/accounts/{account}/trash/hidden",
        get(list_hidden),
    )
    .route(
        "/api/v1/orgs/{org}/accounts/{account}/trash/hidden/{message}/restore",
        post(restore_hidden),
    )
    .route(
        "/api/v1/orgs/{org}/accounts/{account}/trash-purge",
        get(trash_purge),
    )
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(list_trash, restore, list_hidden, restore_hidden, trash_purge),
    components(schemas(
        TrashEntry,
        TrashPage,
        RestoredMessage,
        PurgeMode,
        TrashPurgeRuleBody,
        TrashPurgeInfo
    ))
)]
pub(crate) struct Doc;

impl From<TrashItem> for TrashEntry {
    fn from(i: TrashItem) -> Self {
        let message = match i.object {
            TrashObject::Message(id) => id.to_string(),
            other => format!("{other:?}"),
        };
        Self {
            message,
            origin: i.origin.to_string(),
            size: i.size,
            received_at: i.received_at,
            deleted_at: i.deleted_at,
            expires_at: i.expires_at,
            subject: None,
            from: None,
        }
    }
}

/// Корзина ящика и действующий срок корзины.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/trash", tag = "trash",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)"), PageQuery),
    responses((status = 200, body = TrashPage), (status = 400, body = Problem), (status = 403, body = Problem),
        (status = 404, body = Problem)))]
async fn list_trash(
    State(a): State<Arc<Admin>>,
    Extension(caller): Extension<Caller>,
    route: MatchedPath,
    uri: Uri,
    Path((org, id)): Path<(TenantId, AccountId)>,
    Query(q): Query<PageQuery>,
) -> ApiResult<Json<TrashPage>> {
    let access = Access {
        caller: &caller,
        org,
        route: route.as_str(),
        path: uri.path(),
    };
    Ok(Json(page(&a, &access, id, &q, false).await?))
}

/// Страница видимых (`hidden = false`) или скрытых элементов корзины и действующий срок корзины.
/// С `include=subject` — тема и отправитель писем страницы: только администратору, с записью в аудит.
async fn page(
    a: &Admin,
    access: &Access<'_>,
    id: AccountId,
    q: &PageQuery,
    hidden: bool,
) -> Result<TrashPage, Error> {
    let org = access.org;
    let subjects = content::requested(q.include.as_deref())?;
    if subjects {
        access.check(a).await?;
    }
    let acc = account(a, org, id).await?;
    let limit = a.cfg.content_page(q.limit, subjects);
    let scope = AccountScope {
        tenant: org,
        account: id,
    };
    let items = if hidden {
        a.d.trash.list_hidden(scope, q.offset, limit).await?
    } else {
        a.d.trash.list(scope, q.offset, limit).await?
    };
    let policy =
        a.d.resolver
            .resolve(org, Some(acc.domain), Some(id), None)
            .await?;
    let next_offset = (items.len() == limit).then(|| q.offset + limit);
    let mut entries = Vec::with_capacity(items.len());
    if subjects {
        access.record(a, 200, items.len()).await?;
    }
    for item in items {
        let message = match item.object {
            TrashObject::Message(m) if subjects => Some(m),
            _ => None,
        };
        let mut entry = TrashEntry::from(item);
        if subjects {
            let head = match message {
                // Скрытое письмо хранилище не отдаёт: тема и отправитель — из метаданных, вычисленных при приёме
                // (MS-19); BLOB не читается, скрытие не снимается.
                Some(m) if hidden => {
                    a.d.trash
                        .hidden_message(scope, m)
                        .await?
                        .map(|msg| {
                            content::envelope_headline(&msg.meta.envelope, a.cfg.subject_max_chars)
                        })
                        .unwrap_or_default()
                }
                Some(m) => {
                    let raw =
                        a.d.store
                            .fetch_raw(scope, m, Some(content::head_range(&a.cfg)))
                            .await;
                    content::headline(&a.cfg, raw).await
                }
                None => content::Headline::default(),
            };
            entry.subject = Some(head.subject);
            entry.from = Some(head.from);
        }
        entries.push(entry);
    }
    Ok(TrashPage {
        trash_retention: policy.trash_retention,
        next_offset,
        items: entries,
    })
}

/// Восстанавливает письмо из корзины; письмо не в корзине — 400.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/accounts/{account}/trash/{message}/restore", tag = "trash",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)"),
        ("message" = String, Path, description = "Письмо (UUID)")),
    responses((status = 200, body = RestoredMessage), (status = 400, body = Problem), (status = 404, body = Problem)))]
async fn restore(
    State(a): State<Arc<Admin>>,
    Path((org, id, message)): Path<(TenantId, AccountId, MessageId)>,
) -> ApiResult<Json<RestoredMessage>> {
    account(&a, org, id).await?;
    let scope = AccountScope {
        tenant: org,
        account: id,
    };
    let done = a.d.store.restore(scope, &[message]).await?;
    let r = done.first().ok_or(Error::NotFound)?;
    Ok(Json(RestoredMessage {
        message: r.id.to_string(),
        uid: r.uid.0,
        modseq: r.modseq.0,
    }))
}

/// Скрытые письма ящика («очистить корзину» с `hide`): пользователь их не видит, хранятся до конца срока корзины.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/trash/hidden", tag = "trash",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)"), PageQuery),
    responses((status = 200, body = TrashPage), (status = 400, body = Problem), (status = 403, body = Problem),
        (status = 404, body = Problem)))]
async fn list_hidden(
    State(a): State<Arc<Admin>>,
    Extension(caller): Extension<Caller>,
    route: MatchedPath,
    uri: Uri,
    Path((org, id)): Path<(TenantId, AccountId)>,
    Query(q): Query<PageQuery>,
) -> ApiResult<Json<TrashPage>> {
    let access = Access {
        caller: &caller,
        org,
        route: route.as_str(),
        path: uri.path(),
    };
    Ok(Json(page(&a, &access, id, &q, true).await?))
}

/// Восстанавливает скрытое (или видимое) письмо корзины в исходную папку (нет — во «Входящие»);
/// письма нет в корзине — 404.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/accounts/{account}/trash/hidden/{message}/restore", tag = "trash",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)"),
        ("message" = String, Path, description = "Письмо (UUID)")),
    responses((status = 200, body = RestoredMessage), (status = 404, body = Problem)))]
async fn restore_hidden(
    State(a): State<Arc<Admin>>,
    Path((org, id, message)): Path<(TenantId, AccountId, MessageId)>,
) -> ApiResult<Json<RestoredMessage>> {
    account(&a, org, id).await?;
    let scope = AccountScope {
        tenant: org,
        account: id,
    };
    match a
        .d
        .trash
        .restore(scope, TrashObject::Message(message))
        .await?
    {
        Restored::Message { inserted, .. } => Ok(Json(RestoredMessage {
            message: inserted.id.to_string(),
            uid: inserted.uid.0,
            modseq: inserted.modseq.0,
        })),
        other => Err(Error::internal(format!(
            "trash: неожиданный результат восстановления {other:?}"
        ))
        .into()),
    }
}

/// Настройка `trash.purge` ящика: правило администратора, выбор пользователя, действующее значение.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/trash-purge", tag = "trash",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    responses((status = 200, body = TrashPurgeInfo), (status = 404, body = Problem)))]
async fn trash_purge(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<Json<TrashPurgeInfo>> {
    account(&a, org, id).await?;
    let scope = AccountScope {
        tenant: org,
        account: id,
    };
    Ok(Json(a.d.resolver.trash_purge(scope).await?.into()))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use tp_contracts::mailstore::MailStore;
    use tp_contracts::{AccountId, MessageId, TenantId};

    use tp_contracts::Error;
    use tp_contracts::ext::DecisionHooks;
    use tp_contracts::meta::{MessageStore, PolicyResolver, SpecialUse};
    use tp_contracts::retention::{TrashPurge, TrashStore};

    use crate::server::AdminConfig;
    use crate::testenv::{Env, config};

    #[tokio::test]
    async fn list_and_restore() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        let mut ids = Vec::new();
        for s in ["one", "two", "three"] {
            ids.push(env.deliver(scope, s).await);
        }
        env.store.delete_to_trash(scope, &ids).await.expect("trash");
        let base = format!(
            "/api/v1/orgs/{}/accounts/{}/trash",
            TenantId::DEFAULT,
            scope.account
        );

        let (s, _, b) = env
            .call("GET", &format!("{base}?limit=2"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["items"].as_array().expect("items").len(), 2);
        assert_eq!(b["next_offset"], 2);
        assert_eq!(b["trash_retention"], Value::Null);
        let item = &b["items"][0];
        assert!(
            item["expires_at"].as_str().expect("expires").ends_with('Z'),
            "{item}"
        );
        let (_, _, b) = env
            .call("GET", &format!("{base}?offset=2"), Some("org"), None)
            .await;
        assert_eq!(b["items"].as_array().expect("items").len(), 1);
        assert_eq!(b["next_offset"], Value::Null);

        let target = ids[1];
        let restore = format!("{base}/{target}/restore");
        let (s, _, b) = env.call("POST", &restore, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["message"], target.to_string());
        let (_, _, b) = env.call("GET", &base, Some("org"), None).await;
        let left: Vec<_> = b["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|i| i["message"].clone())
            .collect();
        assert_eq!(left.len(), 2);
        assert!(!left.contains(&json!(target.to_string())));
        let (s, _, _) = env.call("POST", &restore, Some("org"), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "письмо уже не в корзине");
        let missing = format!("{base}/{}/restore", MessageId::new());
        let (s, _, _) = env.call("POST", &missing, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        let log = env.audit.records();
        let audited = log.iter().find(|r| r.path == restore).expect("audit");
        assert_eq!(audited.status, 200);
        assert_eq!(audited.tenant, Some(TenantId::DEFAULT));
    }

    /// Приёмка ADM-19: тема и отправитель — только по `include=subject`, администратору, с записью в аудит.
    #[tokio::test]
    async fn subjects_on_request() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        let ids = [
            env.deliver(scope, "=?UTF-8?B?0J7RgtGH0ZHRgg==?= Q3").await,
            env.deliver(scope, &"x".repeat(300)).await,
        ];
        env.store.delete_to_trash(scope, &ids).await.expect("trash");
        let base = format!(
            "/api/v1/orgs/{}/accounts/{}/trash",
            TenantId::DEFAULT,
            scope.account
        );
        let content = |env: &Env| -> Vec<_> {
            let log = env.audit.records();
            log.into_iter()
                .filter(|r| r.content_messages.is_some())
                .collect()
        };

        // Без параметра полей нет и записи о доступе к содержимому нет.
        let (s, _, b) = env.call("GET", &base, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let item = b["items"][0].as_object().expect("item");
        assert!(!item.contains_key("subject") && !item.contains_key("from"));
        assert!(content(&env).is_empty());

        let with = format!("{base}?include=subject");
        let (s, _, b) = env.call("GET", &with, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let items = b["items"].as_array().expect("items");
        let by_id = |id: MessageId| {
            items
                .iter()
                .find(|i| i["message"] == id.to_string())
                .expect("item")
        };
        assert_eq!(by_id(ids[0])["subject"], "Отчёт Q3");
        assert_eq!(by_id(ids[0])["from"], "a@example.com");
        let long = by_id(ids[1])["subject"].as_str().expect("subject");
        assert_eq!(long.len(), 200, "обрезано до subject_max_chars");
        let log = content(&env);
        assert_eq!(log.len(), 1);
        let r = &log[0];
        assert_eq!(
            (r.method.as_str(), r.status, r.content_messages),
            ("GET", 200, Some(2))
        );
        assert_eq!(
            (r.route.as_str(), r.path.as_str(), r.actor_login.as_str()),
            (
                "/api/v1/orgs/{org}/accounts/{account}/trash",
                base.as_str(),
                "admin@example.org"
            )
        );
        assert!(
            !serde_json::to_string(r).expect("json").contains("Отчёт"),
            "тем в аудите нет"
        );

        // Оператору содержимое не отдаётся (отказ — в аудите), список без содержимого — да.
        let (s, _, _) = env.call("GET", &with, Some("operator"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env.call("GET", &base, Some("operator"), None).await;
        assert_eq!(s, StatusCode::OK);
        let log = content(&env);
        let last = log.last().expect("audit");
        assert_eq!(
            (log.len(), last.status, last.actor_login.as_str()),
            (2, 403, "ops@example.org")
        );
        let (s, _, _) = env
            .call("GET", &format!("{base}?include=body"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // Страница с содержимым — не больше content_page_size; скрытые письма — так же.
        let cfg = AdminConfig {
            content_page_size: 1,
            subject_max_chars: 5,
            ..config()
        };
        let env = Env::with(cfg, Vec::new());
        let scope = env.account("user@example.org").await;
        let ids = [
            env.deliver(scope, "первое письмо").await,
            env.deliver(scope, "второе письмо").await,
        ];
        env.store.delete_to_trash(scope, &ids).await.expect("trash");
        let base = format!(
            "/api/v1/orgs/{}/accounts/{}/trash",
            TenantId::DEFAULT,
            scope.account
        );
        let page = format!("{base}?include=subject&limit=50");
        let (s, _, b) = env.call("GET", &page, Some("root"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let items = b["items"].as_array().expect("items");
        assert_eq!((items.len(), &b["next_offset"]), (1, &json!(1)));
        let subject = items[0]["subject"].as_str().expect("subject");
        assert!(["перво", "второ"].contains(&subject), "{subject}");
        assert_eq!(content(&env)[0].content_messages, Some(1));

        // Скрытое письмо: тема и отправитель — из метаданных письма (MS-19), а не из хранилища.
        env.trash
            .purge(scope, &ids, &DecisionHooks::default())
            .await
            .expect("purge");
        let hidden = format!("{base}/hidden?include=subject");
        let (s, _, b) = env.call("GET", &hidden, Some("root"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let item = b["items"][0].as_object().expect("item");
        let subject = item["subject"].as_str().expect("subject");
        assert!(["перво", "второ"].contains(&subject), "{subject}");
        assert!(
            item["from"].as_str().is_some_and(|f| !f.is_empty()),
            "{item:?}"
        );
        assert_eq!(content(&env).len(), 2);
    }

    /// Срок корзины — из политики организации, домена или ящика (самый специфичный уровень).
    #[tokio::test]
    async fn retention_by_policy() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        let org = format!("/api/v1/orgs/{}", TenantId::DEFAULT);
        let acc = format!("{org}/accounts/{}", scope.account);
        let trash = format!("{acc}/trash");
        let retention = |b: &Value| b["trash_retention"].clone();

        let p = json!({"trash_retention": "30d"});
        env.call("PUT", &format!("{org}/policy"), Some("org"), Some(p))
            .await;
        let (_, _, b) = env.call("GET", &trash, Some("org"), None).await;
        assert_eq!(retention(&b), "30d");
        let p = json!({"trash_retention": "7d", "quota_bytes": 10});
        env.call("PUT", &format!("{acc}/policy"), Some("org"), Some(p))
            .await;
        let (_, _, b) = env.call("GET", &trash, Some("org"), None).await;
        assert_eq!(retention(&b), "7d");
        env.call(
            "PUT",
            &format!("{acc}/policy"),
            Some("org"),
            Some(json!({})),
        )
        .await;
        let (_, _, b) = env.call("GET", &trash, Some("org"), None).await;
        assert_eq!(retention(&b), "30d");
    }

    #[tokio::test]
    async fn access_and_paging_limits() {
        let cfg = AdminConfig {
            page_size: 1,
            max_page_size: 2,
            ..config()
        };
        let env = Env::with(cfg, vec![]);
        let scope = env.account("user@example.org").await;
        let ids = [
            env.deliver(scope, "a").await,
            env.deliver(scope, "b").await,
            env.deliver(scope, "c").await,
        ];
        env.store.delete_to_trash(scope, &ids).await.expect("trash");
        let base = format!(
            "/api/v1/orgs/{}/accounts/{}/trash",
            TenantId::DEFAULT,
            scope.account
        );
        let (_, _, b) = env.call("GET", &base, Some("org"), None).await;
        assert_eq!(b["items"].as_array().expect("items").len(), 1);
        let (_, _, b) = env
            .call("GET", &format!("{base}?limit=50"), Some("org"), None)
            .await;
        assert_eq!(b["items"].as_array().expect("items").len(), 2);

        let (s, _, _) = env.call("GET", &base, Some("user"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env.call("GET", &base, Some("other"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let other = format!(
            "/api/v1/orgs/{}/accounts/{}/trash",
            TenantId::DEFAULT,
            AccountId::new()
        );
        let (s, _, _) = env.call("GET", &other, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    /// Администратор задаёт `trash.purge`; без `user_can_choose` выбор пользователя не действует.
    #[tokio::test]
    async fn trash_purge_rule() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        let org = format!("/api/v1/orgs/{}", TenantId::DEFAULT);
        let policy = format!("{org}/policy");
        let setting = format!("{org}/accounts/{}/trash-purge", scope.account);
        let get = || async { env.call("GET", &setting, Some("org"), None).await };

        let (s, _, b) = get().await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            b,
            json!({"rule": {"mode": "hide", "user_can_choose": false}, "user": null, "effective": "hide"})
        );

        let erase = json!({"trash_purge": {"mode": "erase"}});
        let (s, _, b) = env
            .call("PUT", &policy, Some("org"), Some(erase.clone()))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            b["trash_purge"],
            json!({"mode": "erase", "user_can_choose": false})
        );
        let (_, _, b) = env.call("GET", &org, Some("org"), None).await;
        assert_eq!(b["effective"]["trash_purge"]["mode"], "erase");
        let denied = env
            .policies
            .set_user_trash_purge(scope, Some(TrashPurge::Hide))
            .await;
        assert!(matches!(denied, Err(Error::Forbidden)), "{denied:?}");
        let (_, _, b) = get().await;
        assert_eq!(b["user"], Value::Null);
        assert_eq!(b["effective"], "erase");

        let choose = json!({"trash_purge": {"mode": "erase", "user_can_choose": true}});
        env.call("PUT", &policy, Some("org"), Some(choose)).await;
        env.policies
            .set_user_trash_purge(scope, Some(TrashPurge::Hide))
            .await
            .expect("user choice");
        let (_, _, b) = get().await;
        assert_eq!(b["user"], "hide");
        assert_eq!(b["effective"], "hide");

        // Администратор отозвал выбор: его значение приоритетнее сохранённого выбора пользователя.
        env.call("PUT", &policy, Some("org"), Some(erase)).await;
        let (_, _, b) = get().await;
        assert_eq!(b["user"], "hide");
        assert_eq!(b["effective"], "erase");
        let id = env.deliver(scope, "gone").await;
        env.store
            .delete_to_trash(scope, &[id])
            .await
            .expect("trash");
        let out = env
            .trash
            .purge(scope, &[id], &DecisionHooks::default())
            .await
            .expect("purge");
        assert_eq!(out.erased, vec![id]);

        for bad in [
            json!({"trash_purge": {"mode": "drop"}}),
            json!({"trash_purge": {"user_can_choose": true}}),
            json!({"trash_purge": {"mode": "hide", "extra": 1}}),
        ] {
            let (s, _, _) = env.call("PUT", &policy, Some("org"), Some(bad)).await;
            assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        }
        let (s, _, _) = env.call("GET", &setting, Some("user"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let missing = format!("{org}/accounts/{}/trash-purge", AccountId::new());
        let (s, _, _) = env.call("GET", &missing, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let log = env.audit.records();
        assert!(log.iter().any(|r| r.path == policy && r.status == 200));
    }

    /// Скрытое «очисткой корзины» письмо видно администратору и восстанавливается в исходную папку.
    #[tokio::test]
    async fn hidden_list_and_restore() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        let (a, b) = (env.deliver(scope, "a").await, env.deliver(scope, "b").await);
        env.store
            .delete_to_trash(scope, &[a, b])
            .await
            .expect("trash");
        let out = env
            .trash
            .purge(scope, &[a], &DecisionHooks::default())
            .await
            .expect("purge");
        assert_eq!(out.hidden, vec![a]);
        let base = format!(
            "/api/v1/orgs/{}/accounts/{}/trash",
            TenantId::DEFAULT,
            scope.account
        );
        let hidden = format!("{base}/hidden");
        let ids = |v: &Value| -> Vec<Value> {
            v["items"]
                .as_array()
                .expect("items")
                .iter()
                .map(|i| i["message"].clone())
                .collect()
        };

        let (_, _, v) = env.call("GET", &base, Some("org"), None).await;
        assert_eq!(ids(&v), vec![json!(b.to_string())]);
        let (s, _, v) = env.call("GET", &hidden, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(ids(&v), vec![json!(a.to_string())]);
        assert_eq!(v["next_offset"], Value::Null);
        // Без `include=subject` темы и отправителя нет, и чтение в аудит не пишется (ADM-32).
        let item = v["items"][0].as_object().expect("item");
        assert!(
            !item.contains_key("subject") && !item.contains_key("from"),
            "{item:?}"
        );
        assert!(
            env.audit
                .records()
                .iter()
                .all(|r| r.content_messages.is_none())
        );

        let restore = format!("{hidden}/{a}/restore");
        let (s, _, v) = env.call("POST", &restore, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["message"], a.to_string());
        let (_, _, v) = env.call("GET", &hidden, Some("org"), None).await;
        assert!(ids(&v).is_empty());
        let inbox = env
            .meta
            .mailbox_by_role(scope, SpecialUse::Inbox)
            .expect("inbox");
        let msg = MessageStore::get(&env.meta, scope, a)
            .await
            .expect("get")
            .expect("restored");
        assert_eq!(msg.mailbox, inbox);
        let (s, _, _) = env.call("POST", &restore, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "письмо уже не в корзине");

        let (s, _, _) = env.call("GET", &hidden, Some("user"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env.call("POST", &restore, Some("operator"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let log = env.audit.records();
        let audited: Vec<_> = log.iter().filter(|r| r.path == restore).collect();
        assert_eq!(audited.len(), 3);
        assert_eq!(audited[0].status, 200);
        assert_eq!(audited[0].tenant, Some(TenantId::DEFAULT));
    }
}
