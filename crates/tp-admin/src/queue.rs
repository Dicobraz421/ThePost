//! Очередь исходящей почты организации (ADM-45; контракт `QueueStore`, CTR-85): просмотр записей и повтор доставки.
//!
//! - `GET /api/v1/orgs/{org}/queue` — записи по возрастанию идентификатора ([`QueueStore::list`]); фильтры — состояние
//!   (`status`) и подстрока адреса отправителя или получателя (`address`); продолжение — заголовок
//!   `Link: <…&after=…>; rel="next"` (RFC 8288), как у списков учётных записей и очереди модерации.
//! - `POST /api/v1/orgs/{org}/queue/{id}/retry` — попытка «сейчас» у всех получателей без конечного статуса
//!   ([`QueueStore::retry`]): 202. Запись доставляется (действует лиза узла smtp-out) — 409 problem+json с пояснением:
//!   срок не перенесён, повторять нечего, пока идёт попытка; нет записи (в том числе уже доставлена) — 404.
//!
//! Права — общие правила ([`crate::server::AdminRole::permits`]): администраторы организации и инсталляции, оператор —
//! только список; повтор, как каждое изменение, пишет в журнал аудита слой доступа. Очередь не подключена
//! ([`Admin::with_queue`]) — 503.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tp_contracts::queue::{
    QueueFilter, QueueItem, QueueItemRcpt, QueueRcptState, QueueState, QueueStore, RcptStatus,
};
use tp_contracts::{Error, QueueId, TenantId};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::accounts::encode_query;
use crate::extract::{Json, Path, Query};
use crate::server::{Admin, ApiResult, Problem, Refused};

/// Состояние записи очереди: `ready` — ждёт первой попытки (в том числе отложенная отправка), `retry` — ждёт повтора
/// после временной неудачи, `active` — доставляется (действует лиза узла smtp-out).
#[allow(dead_code)]
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = QueueStatus)]
pub enum QueueStatusSchema {
    /// Ждёт первой попытки.
    Ready,
    /// Ждёт повтора.
    Retry,
    /// Доставляется.
    Active,
}

/// Состояние получателя: `pending` — ждёт попытки, `delivered` — доставлено, `failed` — окончательный отказ.
#[allow(dead_code)]
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = QueueRcptStatus)]
pub enum QueueRcptStatusSchema {
    /// Ждёт попытки.
    Pending,
    /// Доставлено.
    Delivered,
    /// Окончательный отказ.
    Failed,
}

/// Получатель записи очереди.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct QueueRcptInfo {
    /// Адрес (`RCPT TO`).
    pub address: String,
    /// Состояние.
    #[schema(value_type = QueueRcptStatusSchema)]
    pub status: String,
    /// Срок следующей попытки (для `pending`).
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub next_attempt: Option<OffsetDateTime>,
    /// Ответ принимающего сервера или причина отказа (для `delivered` и `failed`).
    pub reply: Option<String>,
    /// Число неудачных попыток.
    pub attempts: u32,
    /// Причина последней неудачи.
    pub last_error: Option<String>,
}

impl From<QueueItemRcpt> for QueueRcptInfo {
    fn from(r: QueueItemRcpt) -> Self {
        let (status, next_attempt, reply) = match r.state {
            QueueRcptState::Pending { next_attempt } => ("pending", Some(next_attempt), None),
            QueueRcptState::Done(RcptStatus::Delivered { reply }) => {
                ("delivered", None, Some(reply))
            }
            QueueRcptState::Done(RcptStatus::Failed { reply }) => ("failed", None, Some(reply)),
        };
        Self {
            address: r.rcpt.address,
            status: status.to_owned(),
            next_attempt,
            reply,
            attempts: r.attempts,
            last_error: r.last_error,
        }
    }
}

/// Запись очереди исходящей почты.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct QueueEntry {
    /// Запись (UUID).
    pub id: String,
    /// Состояние в момент запроса.
    #[schema(value_type = QueueStatusSchema)]
    pub status: QueueState,
    /// Момент постановки (для отложенного письма — срок выпуска, если он позже).
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub queued_at: OffsetDateTime,
    /// Ближайший срок попытки среди получателей без конечного статуса.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub next_attempt: OffsetDateTime,
    /// Отправитель конверта; пустой — уведомление о недоставке.
    pub from: String,
    /// Размер письма, байт.
    pub size: u64,
    /// Получатели в порядке постановки.
    pub rcpts: Vec<QueueRcptInfo>,
}

impl From<QueueItem> for QueueEntry {
    fn from(i: QueueItem) -> Self {
        Self {
            id: i.id.to_string(),
            status: i.state,
            queued_at: i.queued_at,
            next_attempt: i.next_attempt,
            from: i.from,
            size: i.size,
            rcpts: i.rcpts.into_iter().map(Into::into).collect(),
        }
    }
}

/// Список очереди: фильтры и страница.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct QueueQuery {
    /// Только записи в этом состоянии: `ready`, `retry`, `active`.
    #[param(value_type = Option<QueueStatusSchema>)]
    pub status: Option<QueueState>,
    /// Только записи, у которых отправитель или получатель содержит эту строку (без учёта регистра).
    pub address: Option<String>,
    /// Размер страницы (по умолчанию и наибольший — из конфигурации).
    pub limit: Option<usize>,
    /// Продолжение после этой записи — `after` из ссылки `rel="next"` предыдущей страницы.
    #[param(value_type = Option<String>)]
    pub after: Option<QueueId>,
}

/// Маршруты очереди.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route("/api/v1/orgs/{org}/queue", get(list_queue))
        .route("/api/v1/orgs/{org}/queue/{id}/retry", post(retry_queue))
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(list_queue, retry_queue),
    components(schemas(QueueEntry, QueueRcptInfo, QueueStatusSchema, QueueRcptStatusSchema))
)]
pub(crate) struct Doc;

/// Очередь организации; не подключена — 503.
fn store(a: &Admin) -> ApiResult<&dyn QueueStore> {
    Ok(a.queue.as_deref().ok_or(Error::UNAVAILABLE)?)
}

/// Записи очереди организации по возрастанию идентификатора. Есть продолжение — заголовок
/// `Link: <…&after=…>; rel="next"` (RFC 8288), нет заголовка — страница последняя.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/queue", tag = "queue",
    params(("org" = String, Path, description = "Организация (UUID)"), QueueQuery),
    responses(
        (status = 200, body = Vec<QueueEntry>, headers(("Link" = String,
            description = "Следующая страница очереди: `<ссылка>; rel=\"next\"`; нет — страница последняя"))),
        (status = 400, body = Problem), (status = 403, body = Problem), (status = 503, body = Problem)))]
async fn list_queue(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Query(q): Query<QueueQuery>,
) -> ApiResult<(HeaderMap, Json<Vec<QueueEntry>>)> {
    let limit = a.cfg.page(q.limit);
    let address = q
        .address
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let filter = QueueFilter {
        state: q.status,
        address,
    };
    // На одну запись больше: продолжение есть, только если после страницы записи действительно есть.
    let mut items = store(&a)?
        .list(
            org,
            &filter,
            q.after,
            limit.saturating_add(1),
            OffsetDateTime::now_utc(),
        )
        .await?;
    let mut headers = HeaderMap::new();
    if items.len() > limit {
        items.truncate(limit);
        if let Some(last) = items.last() {
            let value = HeaderValue::from_str(&next_link(org, &filter, last.id, limit))
                .map_err(|e| Error::internal(format!("ссылка Link: {e}")))?;
            headers.insert(header::LINK, value);
        }
    }
    Ok((headers, Json(items.into_iter().map(Into::into).collect())))
}

/// Ссылка на следующую страницу очереди (RFC 8288; путь от корня Admin API) с теми же фильтрами.
fn next_link(org: TenantId, filter: &QueueFilter, after: QueueId, limit: usize) -> String {
    let mut q = String::new();
    if let Some(s) = filter.state {
        let name = match s {
            QueueState::Ready => "ready",
            QueueState::Retry => "retry",
            QueueState::Active => "active",
        };
        q.push_str(&format!("status={name}&"));
    }
    if let Some(addr) = &filter.address {
        q.push_str(&format!("address={}&", encode_query(addr)));
    }
    format!("</api/v1/orgs/{org}/queue?{q}limit={limit}&after={after}>; rel=\"next\"")
}

/// Повтор доставки «сейчас»: срок попытки всех получателей без конечного статуса — момент запроса, отложенное
/// письмо выпускается. Запись доставляется (действует лиза) — 409: попытка уже идёт, срок не перенесён.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/queue/{id}/retry", tag = "queue",
    params(("org" = String, Path, description = "Организация (UUID)"), ("id" = String, Path, description = "Запись очереди (UUID)")),
    responses((status = 202), (status = 403, body = Problem), (status = 404, body = Problem),
        (status = 409, body = Problem), (status = 503, body = Problem)))]
async fn retry_queue(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, QueueId)>,
) -> std::result::Result<StatusCode, Refused> {
    let store = a.queue.as_deref().ok_or(Error::UNAVAILABLE)?;
    if store.retry(org, id, OffsetDateTime::now_utc()).await? {
        return Ok(StatusCode::ACCEPTED);
    }
    Err(Refused::Problem(
        StatusCode::CONFLICT,
        "запись доставляется: попытка уже идёт, срок не перенесён; повторите после её завершения"
            .to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{StatusCode, header};
    use serde_json::Value;
    use time::OffsetDateTime;
    use tp_contracts::queue::{OutMessage, OutRcpt, QueueStore};
    use tp_contracts::{BlobHash, BlobKey, Gen, QueueId, TenantId};

    use crate::testenv::Env;

    fn queue_path(tenant: TenantId) -> String {
        format!("/api/v1/orgs/{tenant}/queue")
    }

    /// Ставит письмо организации `tenant` от `from` получателям `rcpts`.
    async fn enqueue(env: &Env, tenant: TenantId, from: &str, rcpts: &[&str]) -> QueueId {
        env.queue
            .enqueue(
                None,
                OutMessage {
                    tenant,
                    blob: BlobKey {
                        tenant,
                        hash: BlobHash([7; 32]),
                        generation: Gen::INITIAL,
                    },
                    size: 100,
                    from: from.into(),
                    dsn: Default::default(),
                    rcpts: rcpts.iter().map(|r| OutRcpt::new(*r)).collect(),
                    next_attempt: OffsetDateTime::now_utc() + time::Duration::hours(1),
                },
            )
            .await
            .expect("enqueue")
    }

    /// Путь ссылки `Link: <…>; rel="next"`.
    fn next(h: &axum::http::HeaderMap) -> Option<String> {
        let v = h.get(header::LINK)?.to_str().expect("link");
        let path = v.strip_prefix('<')?.strip_suffix(">; rel=\"next\"")?;
        Some(path.to_owned())
    }

    fn ids(b: &Value) -> Vec<String> {
        b.as_array()
            .expect("entries")
            .iter()
            .map(|e| e["id"].as_str().expect("id").to_owned())
            .collect()
    }

    /// Приёмка ADM-45: список с фильтром по адресу и второй страницей по `Link`; фильтр по состоянию.
    #[tokio::test]
    async fn list_filters_and_pages() {
        let env = Env::new();
        let d = TenantId::DEFAULT;
        let a = enqueue(&env, d, "alice@example.org", &["x@remote.example"]).await;
        let _ = enqueue(&env, d, "bob@example.org", &["y@remote.example"]).await;
        let c = enqueue(
            &env,
            d,
            "carol@example.org",
            &["X@Remote.Example", "z@other.example"],
        )
        .await;
        let e = enqueue(&env, d, "dave@example.org", &["x@remote.example"]).await;
        enqueue(
            &env,
            env.other_tenant,
            "eve@example.org",
            &["x@remote.example"],
        )
        .await;

        // Фильтр по адресу (без учёта регистра), страницы по 2: вторая — по ссылке `Link` с теми же фильтрами.
        let url = format!("{}?address=x%40REMOTE.example&limit=2", queue_path(d));
        let (s, h, b) = env.call("GET", &url, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(ids(&b), [a.to_string(), c.to_string()]);
        let first = &b[0];
        assert_eq!(first["status"], "ready");
        assert_eq!(first["from"], "alice@example.org");
        assert_eq!(first["size"], 100);
        assert_eq!(first["rcpts"][0]["status"], "pending");
        assert_eq!(first["rcpts"][0]["attempts"], 0);
        assert!(first["rcpts"][0]["next_attempt"].is_string(), "{b}");
        let link = next(&h).expect("rel=next");
        assert!(
            link.contains("address=x@REMOTE.example&limit=2&after="),
            "{link}"
        );
        let (s, h, b) = env.call("GET", &link, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(ids(&b), [e.to_string()]);
        assert!(next(&h).is_none(), "последняя страница");

        // Состояние: захваченная запись — `active`, остальные — `ready`.
        let now = OffsetDateTime::now_utc() + time::Duration::hours(2);
        let claimed = env.queue.claim(now, 1, Duration::from_secs(86_400)).await;
        let claimed = claimed.expect("claim")[0].claim.id;
        let (_, _, b) = env
            .call(
                "GET",
                &format!("{}?status=active", queue_path(d)),
                Some("operator"),
                None,
            )
            .await;
        assert_eq!(ids(&b), [claimed.to_string()], "{b}");
        let (_, _, b) = env
            .call(
                "GET",
                &format!("{}?status=ready", queue_path(d)),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(ids(&b).len(), 3, "{b}");

        let (s, _, _) = env
            .call(
                "GET",
                &format!("{}?status=sent", queue_path(d)),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, _) = env.call("GET", &queue_path(d), Some("auditor"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    /// Приёмка ADM-45: повтор — 202 и запись аудита; запись под лизой — 409; чужая организация и нет записи — 404.
    #[tokio::test]
    async fn retry_is_accepted_and_audited() {
        let env = Env::new();
        let d = TenantId::DEFAULT;
        let id = enqueue(&env, d, "alice@example.org", &["x@remote.example"]).await;
        let retry = format!("{}/{id}/retry", queue_path(d));

        let (s, _, b) = env.call("POST", &retry, Some("org"), None).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b}");
        let due = env.queue.next_due().await.expect("next_due").expect("due");
        assert!(
            due <= OffsetDateTime::now_utc(),
            "срок попытки — сейчас: {due}"
        );
        let last = env.audit.records().pop().expect("audit");
        assert_eq!(
            (
                last.method.as_str(),
                last.route.as_str(),
                last.status,
                last.tenant
            ),
            ("POST", "/api/v1/orgs/{org}/queue/{id}/retry", 202, Some(d))
        );

        // Запись доставляется — 409 problem+json с пояснением, тоже в аудите.
        let now = OffsetDateTime::now_utc();
        env.queue
            .claim(now, 1, Duration::from_secs(86_400))
            .await
            .expect("claim");
        let (s, h, b) = env.call("POST", &retry, Some("org"), None).await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert_eq!(h[header::CONTENT_TYPE], "application/problem+json");
        assert!(
            b["detail"]
                .as_str()
                .is_some_and(|t| t.contains("доставляется")),
            "{b}"
        );
        assert_eq!(env.audit.records().pop().expect("audit").status, 409);

        // Запись другой организации по пути своей — 404; чужая организация — 403 администратору организации, 404
        // администратору инсталляции; оператор не повторяет.
        let theirs = enqueue(
            &env,
            env.other_tenant,
            "eve@example.org",
            &["x@remote.example"],
        )
        .await;
        let (s, h, _) = env
            .call(
                "POST",
                &format!("{}/{theirs}/retry", queue_path(d)),
                Some("root"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_eq!(h[header::CONTENT_TYPE], "application/problem+json");
        let (s, _, _) = env
            .call(
                "POST",
                &format!("{}/{}/retry", queue_path(d), QueueId::new()),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let other = format!("{}/{theirs}/retry", queue_path(env.other_tenant));
        let (s, _, _) = env.call("POST", &other, Some("org"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, b) = env.call("POST", &other, Some("root"), None).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b}");
        let (s, _, _) = env.call("POST", &retry, Some("operator"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env
            .call(
                "POST",
                &format!("{}/nope/retry", queue_path(d)),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(
            env.audit.records().len(),
            8,
            "каждый POST — в журнале аудита"
        );
    }

    /// Очередь не подключена — 503.
    #[tokio::test]
    async fn without_queue_unavailable() {
        let env = Env::without_queue();
        let d = TenantId::DEFAULT;
        let (s, _, _) = env.call("GET", &queue_path(d), Some("org"), None).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        let path = format!("{}/{}/retry", queue_path(d), QueueId::new());
        let (s, _, _) = env.call("POST", &path, Some("org"), None).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    }
}
