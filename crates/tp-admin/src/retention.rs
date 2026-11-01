//! Корзина ящика (SRS 4.7, 5.2): список и восстановление элементов администратором.
//!
//! Список — `TrashStore::list` (новые первыми, страницы `offset`/`limit`) и действующий срок корзины ящика
//! (`trash_retention` цепочки организация → домен → ящик; `null` — значение по умолчанию конфигурации хранилища).
//! Срок задаётся политиками уровней ([`crate::accounts`]) и действует на элементы, удалённые после изменения.
//! Восстановление — `MailStore::restore`: письмо возвращается в исходную папку (нет — во «Входящие») с тем же
//! идентификатором, клиенты получают уведомление.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::meta::AccountScope;
use tp_contracts::retention::{TrashItem, TrashObject};
use tp_contracts::{AccountId, Error, MessageId, TenantId};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::accounts::account;
use crate::server::{Admin, ApiResult, Problem};

/// Страница списка.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct PageQuery {
    /// Смещение от начала списка.
    #[serde(default)]
    pub offset: usize,
    /// Размер страницы (по умолчанию и наибольший — из конфигурации).
    pub limit: Option<usize>,
}

/// Элемент корзины.
#[derive(Debug, Serialize, ToSchema)]
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
}

/// Страница корзины.
#[derive(Debug, Serialize, ToSchema)]
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
#[derive(Debug, Serialize, ToSchema)]
pub struct RestoredMessage {
    /// Письмо.
    pub message: String,
    /// Новый UID в папке.
    pub uid: u32,
    /// `ModSeq` изменения.
    pub modseq: u64,
}

/// Маршруты корзины.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route("/api/v1/orgs/{org}/accounts/{account}/trash", get(list))
        .route(
            "/api/v1/orgs/{org}/accounts/{account}/trash/{message}/restore",
            post(restore),
        )
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(list, restore),
    components(schemas(TrashEntry, TrashPage, RestoredMessage))
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
        }
    }
}

/// Корзина ящика и действующий срок корзины.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/trash", tag = "trash",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)"), PageQuery),
    responses((status = 200, body = TrashPage), (status = 404, body = Problem)))]
async fn list(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
    Query(q): Query<PageQuery>,
) -> ApiResult<Json<TrashPage>> {
    let acc = account(&a, org, id).await?;
    let limit = a.cfg.page(q.limit);
    let scope = AccountScope {
        tenant: org,
        account: id,
    };
    let items = a.d.trash.list(scope, q.offset, limit).await?;
    let policy =
        a.d.resolver
            .resolve(org, Some(acc.domain), Some(id), None)
            .await?;
    Ok(Json(TrashPage {
        trash_retention: policy.trash_retention,
        next_offset: (items.len() == limit).then(|| q.offset + limit),
        items: items.into_iter().map(TrashEntry::from).collect(),
    }))
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

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use tp_contracts::mailstore::MailStore;
    use tp_contracts::{AccountId, MessageId, TenantId};

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
}
