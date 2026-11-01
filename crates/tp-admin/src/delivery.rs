//! Поиск в журнале доставки (SRS 5.2; message tracking): `GET /api/v1/orgs/{org}/delivery`.
//!
//! Условия — отправитель, получатель, Message-ID, интервал (по «и»), страницы — по непрозрачной позиции `after`
//! (`next` предыдущей страницы). Источник — [`DeliveryLog::search`](tp_contracts::delivery::DeliveryLog::search)
//! в организации из пути. Права — общие правила чтения ([`crate::server::AdminRole::permits`]): администраторы
//! и оператор только для чтения; аудитор — нет.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tp_contracts::TenantId;
use tp_contracts::delivery::{
    DeliveryCursor, DeliveryDecision, DeliveryEvent, DeliveryEventKind, DeliveryQuery,
};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::server::{Admin, ApiResult, Problem};

/// Параметры поиска.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct DeliveryParams {
    /// Отправитель конверта (точно, без учёта регистра ASCII).
    pub sender: Option<String>,
    /// Один из получателей (точно, без учёта регистра ASCII).
    pub recipient: Option<String>,
    /// Message-ID без угловых скобок (точно).
    pub message_id: Option<String>,
    /// Начало интервала (RFC 3339), включительно.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub since: Option<OffsetDateTime>,
    /// Конец интервала (RFC 3339), не включительно.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub until: Option<OffsetDateTime>,
    /// Размер страницы (по умолчанию и наибольший — из конфигурации).
    pub limit: Option<usize>,
    /// Продолжение: `next` предыдущей страницы с теми же условиями.
    pub after: Option<String>,
}

/// Событие журнала доставки.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeliveryEntry {
    /// Момент события.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub at: OffsetDateTime,
    /// Узел.
    pub node: String,
    /// Событие: `receive`, `reject`, `deliver`, `relay`, `dsn`.
    #[schema(value_type = String)]
    pub kind: DeliveryEventKind,
    /// Отправитель конверта; пустой — уведомление о недоставке.
    pub sender: String,
    /// Получатели события.
    pub recipients: Vec<String>,
    /// Message-ID.
    pub message_id: Option<String>,
    /// Запись очереди исходящей почты.
    pub queue_id: Option<String>,
    /// Решение: `accept`, `reject`, `defer`, `quarantine`, `discard`, `hold`.
    #[schema(value_type = String)]
    pub decision: DeliveryDecision,
    /// Причина решения.
    pub reason: Option<String>,
}

impl From<DeliveryEvent> for DeliveryEntry {
    fn from(e: DeliveryEvent) -> Self {
        Self {
            at: e.at,
            node: e.node.to_string(),
            kind: e.kind,
            sender: e.sender,
            recipients: e.recipients,
            message_id: e.message_id,
            queue_id: e.queue_id.map(|q| q.to_string()),
            decision: e.decision,
            reason: e.reason,
        }
    }
}

/// Страница результатов.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeliveryResults {
    /// События по возрастанию времени.
    pub events: Vec<DeliveryEntry>,
    /// `after` следующей страницы; `null` — записей больше нет.
    pub next: Option<String>,
}

/// Маршруты журнала доставки.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route("/api/v1/orgs/{org}/delivery", get(search))
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(paths(search), components(schemas(DeliveryEntry, DeliveryResults)))]
pub(crate) struct Doc;

/// Поиск в журнале доставки организации.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/delivery", tag = "delivery",
    params(("org" = String, Path, description = "Организация (UUID)"), DeliveryParams),
    responses((status = 200, body = DeliveryResults), (status = 400, body = Problem), (status = 403, body = Problem)))]
async fn search(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Query(q): Query<DeliveryParams>,
) -> ApiResult<Json<DeliveryResults>> {
    let query = DeliveryQuery {
        sender: q.sender,
        recipient: q.recipient,
        message_id: q.message_id,
        since: q.since,
        until: q.until,
        limit: a.cfg.page(q.limit),
        after: q.after.map(DeliveryCursor),
    };
    query.validate()?;
    let page = a.d.delivery.search(org, &query).await?;
    Ok(Json(DeliveryResults {
        events: page.events.into_iter().map(Into::into).collect(),
        next: page.next.map(|c| c.0),
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    use tp_contracts::TenantId;
    use tp_contracts::delivery::{DeliveryDecision, DeliveryEvent, DeliveryEventKind, DeliveryLog};
    use tp_contracts::membership::NodeId;

    use crate::testenv::Env;

    fn event(
        tenant: TenantId,
        at: OffsetDateTime,
        sender: &str,
        rcpt: &str,
        id: &str,
    ) -> DeliveryEvent {
        DeliveryEvent {
            tenant,
            at,
            node: NodeId::new("mx1").expect("node"),
            kind: DeliveryEventKind::Deliver,
            sender: sender.into(),
            recipients: vec![rcpt.into()],
            message_id: Some(id.into()),
            queue_id: None,
            decision: DeliveryDecision::Accept,
            reason: Some("250 2.0.0 OK".into()),
        }
    }

    fn at(s: &str) -> OffsetDateTime {
        OffsetDateTime::parse(s, &Rfc3339).expect("time")
    }

    fn url(q: &str) -> String {
        format!("/api/v1/orgs/{}/delivery?{q}", TenantId::DEFAULT)
    }

    /// Приёмка ADM-05: поиск по отправителю, получателю, Message-ID и интервалу, страницы, права.
    #[tokio::test]
    async fn search_filters_pages_and_rights() {
        let env = Env::new();
        let d = TenantId::DEFAULT;
        let log = env.delivery.clone();
        for e in [
            event(
                d,
                at("2026-11-01T10:00:00Z"),
                "a@ext.example",
                "u1@example.org",
                "m1@ext",
            ),
            event(
                d,
                at("2026-11-01T11:00:00Z"),
                "a@ext.example",
                "u2@example.org",
                "m2@ext",
            ),
            event(
                d,
                at("2026-11-01T12:00:00Z"),
                "b@ext.example",
                "u1@example.org",
                "m3@ext",
            ),
            event(
                env.other_tenant,
                at("2026-11-01T10:30:00Z"),
                "a@ext.example",
                "x@other.example",
                "m4@ext",
            ),
        ] {
            log.record(e).await.expect("record");
        }
        let ids = |b: &serde_json::Value| -> Vec<String> {
            b["events"]
                .as_array()
                .expect("events")
                .iter()
                .map(|e| e["message_id"].as_str().expect("id").to_owned())
                .collect()
        };

        let (s, _, b) = env
            .call("GET", &url("sender=A@ext.example"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(ids(&b), ["m1@ext", "m2@ext"], "только своя организация");
        assert_eq!(b["events"][0]["kind"], "deliver");
        assert_eq!(b["events"][0]["decision"], "accept");
        assert_eq!(b["events"][0]["node"], "mx1");
        assert_eq!(b["events"][0]["at"], "2026-11-01T10:00:00Z");
        let (_, _, b) = env
            .call("GET", &url("recipient=u1@example.org"), Some("org"), None)
            .await;
        assert_eq!(ids(&b), ["m1@ext", "m3@ext"]);
        let (_, _, b) = env
            .call("GET", &url("message_id=m2@ext"), Some("operator"), None)
            .await;
        assert_eq!(ids(&b), ["m2@ext"], "оператор только для чтения ищет");
        let interval = "since=2026-11-01T10:30:00Z&until=2026-11-01T12:00:00Z";
        let (_, _, b) = env.call("GET", &url(interval), Some("org"), None).await;
        assert_eq!(ids(&b), ["m2@ext"]);

        // Страницы по позиции.
        let (_, _, b) = env.call("GET", &url("limit=2"), Some("org"), None).await;
        assert_eq!(ids(&b), ["m1@ext", "m2@ext"]);
        let next = b["next"].as_str().expect("next").to_owned();
        let (_, _, b) = env
            .call(
                "GET",
                &url(&format!("limit=2&after={next}")),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(ids(&b), ["m3@ext"]);
        assert!(b["next"].is_null());

        // Ошибки запроса и права.
        let empty = "since=2026-11-01T12:00:00Z&until=2026-11-01T10:00:00Z";
        let (s, _, _) = env.call("GET", &url(empty), Some("org"), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, _) = env.call("GET", &url("after=junk"), Some("org"), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, _) = env.call("GET", &url("color=red"), Some("org"), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, _) = env.call("GET", &url(""), Some("auditor"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env.call("GET", &url(""), Some("user"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, b) = env.call("GET", &url(""), Some("root"), None).await;
        assert_eq!((s, ids(&b).len()), (StatusCode::OK, 3));
    }
}
