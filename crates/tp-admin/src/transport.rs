//! Настройки транспорта (SRS 4.1; CONTRACTS.md#Transport, CTR-24): принятые домены и их вид, доверенные источники,
//! политики доменов назначения.
//!
//! Хранилище — `TransportStore` ([`crate::Admin::with_transport`]; не подключено — 503). Каждая запись несёт
//! `version` снимка, на котором основано изменение (в теле; у `DELETE` — `?version=`): устаревшая — 409, ответ —
//! новая версия. Домены и источники — уровень кластера (одна версия на все организации), политики — уровень
//! организации. Под `/api/v1/orgs/{org}/transport/...` видны и меняются только записи организации; источники
//! кластера (общий шлюз) и полный снимок кластера — `/api/v1/transport...`, только администратор инсталляции.
//! Принятые домены — единственный ресурс доменов организации (решение 94): у домена — политика уровня «домен»
//! цепочки организация → домен → ящик (`…/transport/domains/{id}/policy`).
//! Имена доменов и шаблоны приводятся к канонической форме (`normalize_domain`). Изменения узлы получают по
//! сигналу `TransportChanged` (публикует хранилище). Правила потока почты — Admin API модуля.

use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tp_contracts::transport::{
    AcceptedDomain, ClusterTransport, DestinationPolicy, DomainKind, SourceKind, SourceMatch,
    TlsRequirement, TransportStore, TrustedSource, normalize_domain,
};
use tp_contracts::{DomainId, Error, TenantId};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::accounts::{Policies, PolicyBody, PolicyTarget, policies};
use crate::server::{Admin, ApiResult, Problem};

/// Префикс маршрутов уровня кластера (только администратор инсталляции, [`crate::AdminRole::permits`]).
pub const CLUSTER_PREFIX: &str = "/api/v1/transport";

/// Новая версия снимка после записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Written {
    /// Версия снимка (кластера — домены и источники; организации — политики).
    pub version: u64,
}

/// Версия снимка, на котором основано удаление.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct VersionQuery {
    /// Версия снимка из последнего чтения.
    pub version: u64,
}

/// Принятые домены организации.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DomainList {
    /// Версия снимка кластера.
    pub version: u64,
    /// Домены по имени: `{id, tenant, name, kind}`, `kind` — `"authoritative"` или
    /// `{"internal_relay": {"next_hop": "host[:port]"}}`.
    #[schema(value_type = Vec<Object>)]
    pub domains: Vec<AcceptedDomain>,
}

/// Принятый домен: создание или замена.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainBody {
    /// Версия снимка кластера.
    pub version: u64,
    /// Имя домена (приводится к канонической форме: нижний регистр, без точки в конце, IDN — A-label).
    pub name: String,
    /// Вид: `"authoritative"` (по умолчанию) или `{"internal_relay": {"next_hop": "host[:port]"}}`.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub kind: DomainKind,
}

/// Записанный домен.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DomainWritten {
    /// Новая версия снимка кластера.
    pub version: u64,
    /// Домен.
    #[schema(value_type = Object)]
    pub domain: AcceptedDomain,
}

/// Доверенные источники.
#[derive(Debug, Serialize, ToSchema)]
pub struct SourceList {
    /// Версия снимка кластера.
    pub version: u64,
    /// Источники по имени: `{name, tenant, matcher, kind}`.
    #[schema(value_type = Vec<Object>)]
    pub sources: Vec<TrustedSource>,
}

/// Доверенный источник: создание или замена (имя — в пути).
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceBody {
    /// Версия снимка кластера.
    pub version: u64,
    /// Признаки соединения: `{"networks": ["192.0.2.0/24"]}` или `{"client_cert": {"subject": "mx.example.org"}}`.
    #[schema(value_type = Object)]
    pub matcher: SourceMatch,
    /// Вид: `{"gateway": {"xclient": true, "authserv_id": "mx.example.org"}}` или
    /// `{"application": {"senders": ["@example.org"], "external": false}}` (только у источника организации).
    #[schema(value_type = Object)]
    pub kind: SourceKind,
}

/// Снимок уровня кластера.
#[derive(Debug, Serialize, ToSchema)]
pub struct ClusterView {
    /// Версия снимка.
    pub version: u64,
    /// Принятые домены всех организаций.
    #[schema(value_type = Vec<Object>)]
    pub domains: Vec<AcceptedDomain>,
    /// Доверенные источники кластера и организаций.
    #[schema(value_type = Vec<Object>)]
    pub sources: Vec<TrustedSource>,
}

impl From<ClusterTransport> for ClusterView {
    fn from(c: ClusterTransport) -> Self {
        Self {
            version: c.version,
            domains: c.domains,
            sources: c.sources,
        }
    }
}

/// Политики доменов назначения организации.
#[derive(Debug, Serialize, ToSchema)]
pub struct RouteList {
    /// Версия снимка организации.
    pub version: u64,
    /// Политики по шаблону: `{pattern, tls, smart_host, egress_ip}`.
    #[schema(value_type = Vec<Object>)]
    pub routes: Vec<DestinationPolicy>,
}

/// Политика домена назначения: создание или замена (шаблон — в пути).
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteBody {
    /// Версия снимка организации.
    pub version: u64,
    /// TLS: `"opportunistic"` (по умолчанию), `"required"` или `{"pinned": {"spki_sha256": ["<hex>"]}}`.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub tls: TlsRequirement,
    /// Промежуточный узел (`host` или `host:port`); нет — доставка по MX.
    #[serde(default)]
    pub smart_host: Option<String>,
    /// Адрес исходящих соединений; нет — адрес узла по умолчанию.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub egress_ip: Option<IpAddr>,
}

/// Маршруты настроек транспорта.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route(
        "/api/v1/orgs/{org}/transport/domains",
        get(list_domains).post(create_domain),
    )
    .route(
        "/api/v1/orgs/{org}/transport/domains/{id}",
        put(replace_domain).delete(delete_domain),
    )
    .route(
        "/api/v1/orgs/{org}/transport/domains/{id}/policy",
        get(get_domain_policy).put(put_domain_policy),
    )
    .route("/api/v1/orgs/{org}/transport/sources", get(list_sources))
    .route(
        "/api/v1/orgs/{org}/transport/sources/{name}",
        put(put_org_source).delete(delete_org_source),
    )
    .route("/api/v1/orgs/{org}/transport/routes", get(list_routes))
    .route(
        "/api/v1/orgs/{org}/transport/routes/{pattern}",
        put(put_route).delete(delete_route),
    )
    .route(CLUSTER_PREFIX, get(cluster))
    .route(
        "/api/v1/transport/sources/{name}",
        put(put_cluster_source).delete(delete_cluster_source),
    )
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(
        list_domains,
        create_domain,
        replace_domain,
        delete_domain,
        get_domain_policy,
        put_domain_policy,
        list_sources,
        put_org_source,
        delete_org_source,
        list_routes,
        put_route,
        delete_route,
        cluster,
        put_cluster_source,
        delete_cluster_source
    ),
    components(schemas(
        Written,
        DomainList,
        DomainBody,
        DomainWritten,
        SourceList,
        SourceBody,
        ClusterView,
        RouteList,
        RouteBody
    ))
)]
pub(crate) struct Doc;

fn store(a: &Admin) -> ApiResult<&dyn TransportStore> {
    Ok(a.transport.as_deref().ok_or(Error::UNAVAILABLE)?)
}

/// Снимок кластера, если версия совпадает с `expected` (иначе — `Conflict` до проверки ключа). Запись затем идёт
/// с той же `expected`, поэтому проверки по снимку не устаревают: параллельное изменение даст `Conflict`.
async fn cluster_at(t: &dyn TransportStore, expected: u64) -> ApiResult<ClusterTransport> {
    let snap = t.cluster().await?;
    if snap.version != expected {
        return Err(Error::Conflict.into());
    }
    Ok(snap)
}

/// Шаблон политики в канонической форме: `*`, `*.<домен>` или `<домен>`.
fn pattern(p: &str) -> ApiResult<String> {
    Ok(match p {
        "*" => p.to_owned(),
        _ => match p.strip_prefix("*.") {
            Some(d) => format!("*.{}", normalize_domain(d)?),
            None => normalize_domain(p)?,
        },
    })
}

/// Принятые домены организации и версия снимка кластера.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/transport/domains", tag = "transport",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = DomainList), (status = 503, body = Problem)))]
async fn list_domains(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<DomainList>> {
    let c = store(&a)?.cluster().await?;
    Ok(Json(DomainList {
        version: c.version,
        domains: c.domains.into_iter().filter(|d| d.tenant == org).collect(),
    }))
}

/// Добавляет принятый домен (идентификатор выдаёт сервер). Имя занято — 409.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/transport/domains", tag = "transport",
    params(("org" = String, Path, description = "Организация (UUID)")),
    request_body = DomainBody,
    responses((status = 201, body = DomainWritten), (status = 400, body = Problem),
        (status = 409, description = "устаревшая версия или имя занято", body = Problem),
        (status = 503, body = Problem)))]
async fn create_domain(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Json(body): Json<DomainBody>,
) -> ApiResult<(StatusCode, Json<DomainWritten>)> {
    let t = store(&a)?;
    let domain = AcceptedDomain {
        id: DomainId::new(),
        tenant: org,
        name: normalize_domain(&body.name)?,
        kind: body.kind,
    };
    let version = t.put_domain(body.version, &domain).await?;
    Ok((StatusCode::CREATED, Json(DomainWritten { version, domain })))
}

/// Заменяет имя и вид принятого домена организации. Нет — 404, имя занято — 409.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/transport/domains/{id}", tag = "transport",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("id" = String, Path, description = "Домен (UUID)")),
    request_body = DomainBody,
    responses((status = 200, body = DomainWritten), (status = 400, body = Problem), (status = 404, body = Problem),
        (status = 409, description = "устаревшая версия или имя занято", body = Problem),
        (status = 503, body = Problem)))]
async fn replace_domain(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, DomainId)>,
    Json(body): Json<DomainBody>,
) -> ApiResult<Json<DomainWritten>> {
    let t = store(&a)?;
    let snap = cluster_at(t, body.version).await?;
    if !snap.domains.iter().any(|d| d.id == id && d.tenant == org) {
        return Err(Error::NotFound.into());
    }
    let domain = AcceptedDomain {
        id,
        tenant: org,
        name: normalize_domain(&body.name)?,
        kind: body.kind,
    };
    let version = t.put_domain(body.version, &domain).await?;
    Ok(Json(DomainWritten { version, domain }))
}

/// Удаляет принятый домен организации. Нет — 404.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/transport/domains/{id}", tag = "transport",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("id" = String, Path, description = "Домен (UUID)"), VersionQuery),
    responses((status = 200, body = Written), (status = 404, body = Problem),
        (status = 409, description = "устаревшая версия", body = Problem), (status = 503, body = Problem)))]
async fn delete_domain(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, DomainId)>,
    Query(q): Query<VersionQuery>,
) -> ApiResult<Json<Written>> {
    let t = store(&a)?;
    let snap = cluster_at(t, q.version).await?;
    if !snap.domains.iter().any(|d| d.id == id && d.tenant == org) {
        return Err(Error::NotFound.into());
    }
    let version = t.delete_domain(q.version, id).await?;
    Ok(Json(Written { version }))
}

/// Принятый домен организации в текущем снимке кластера; нет (или домен другой организации) — `NotFound`.
async fn own_domain(a: &Admin, org: TenantId, id: DomainId) -> ApiResult<AcceptedDomain> {
    let snap = store(a)?.cluster().await?;
    Ok(snap
        .domains
        .into_iter()
        .find(|d| d.id == id && d.tenant == org)
        .ok_or(Error::NotFound)?)
}

/// Политика принятого домена и действующая политика (наложение организация → домен). Нет домена — 404.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/transport/domains/{id}/policy", tag = "transport",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("id" = String, Path, description = "Домен (UUID)")),
    responses((status = 200, body = Policies), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn get_domain_policy(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, DomainId)>,
) -> ApiResult<Json<Policies>> {
    own_domain(&a, org, id).await?;
    Ok(Json(
        policies(&a, org, PolicyTarget::Domain(id), Some(id), None).await?,
    ))
}

/// Задаёт политику принятого домена целиком. Нет домена — 404.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/transport/domains/{id}/policy", tag = "transport",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("id" = String, Path, description = "Домен (UUID)")),
    request_body = PolicyBody,
    responses((status = 200, body = PolicyBody), (status = 400, body = Problem), (status = 404, body = Problem),
        (status = 503, body = Problem)))]
async fn put_domain_policy(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, DomainId)>,
    Json(body): Json<PolicyBody>,
) -> ApiResult<Json<PolicyBody>> {
    own_domain(&a, org, id).await?;
    a.d.policies
        .set_policy(org, PolicyTarget::Domain(id), &body.clone().into())
        .await?;
    Ok(Json(body))
}

/// Доверенные источники организации и версия снимка кластера.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/transport/sources", tag = "transport",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = SourceList), (status = 503, body = Problem)))]
async fn list_sources(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<SourceList>> {
    let c = store(&a)?.cluster().await?;
    Ok(Json(SourceList {
        version: c.version,
        sources: c
            .sources
            .into_iter()
            .filter(|s| s.tenant == Some(org))
            .collect(),
    }))
}

/// Запись источника владельца `owner` (`None` — кластер): имя занято другим владельцем — `Conflict`.
async fn put_source(
    a: &Admin,
    owner: Option<TenantId>,
    name: String,
    body: SourceBody,
) -> ApiResult<Json<Written>> {
    let t = store(a)?;
    let snap = cluster_at(t, body.version).await?;
    if snap
        .sources
        .iter()
        .any(|s| s.name == name && s.tenant != owner)
    {
        return Err(Error::Conflict.into());
    }
    let source = TrustedSource {
        name,
        tenant: owner,
        matcher: body.matcher,
        kind: body.kind,
    };
    let version = t.put_source(body.version, &source).await?;
    Ok(Json(Written { version }))
}

/// Удаление источника владельца `owner`: чужой или отсутствующий — `NotFound`.
async fn delete_source(
    a: &Admin,
    owner: Option<TenantId>,
    name: &str,
    expected: u64,
) -> ApiResult<Json<Written>> {
    let t = store(a)?;
    let snap = cluster_at(t, expected).await?;
    if !snap
        .sources
        .iter()
        .any(|s| s.name == name && s.tenant == owner)
    {
        return Err(Error::NotFound.into());
    }
    let version = t.delete_source(expected, name).await?;
    Ok(Json(Written { version }))
}

/// Создаёт или заменяет доверенный источник организации. Имя занято другим владельцем — 409.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/transport/sources/{name}", tag = "transport",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("name" = String, Path, description = "Имя источника ([a-z0-9._-], до 64 символов)")),
    request_body = SourceBody,
    responses((status = 200, body = Written), (status = 400, body = Problem),
        (status = 409, description = "устаревшая версия или имя занято", body = Problem),
        (status = 503, body = Problem)))]
async fn put_org_source(
    State(a): State<Arc<Admin>>,
    Path((org, name)): Path<(TenantId, String)>,
    Json(body): Json<SourceBody>,
) -> ApiResult<Json<Written>> {
    put_source(&a, Some(org), name, body).await
}

/// Удаляет доверенный источник организации. Нет — 404.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/transport/sources/{name}", tag = "transport",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("name" = String, Path, description = "Имя источника"), VersionQuery),
    responses((status = 200, body = Written), (status = 404, body = Problem),
        (status = 409, description = "устаревшая версия", body = Problem), (status = 503, body = Problem)))]
async fn delete_org_source(
    State(a): State<Arc<Admin>>,
    Path((org, name)): Path<(TenantId, String)>,
    Query(q): Query<VersionQuery>,
) -> ApiResult<Json<Written>> {
    delete_source(&a, Some(org), &name, q.version).await
}

/// Политики доменов назначения организации и версия её снимка.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/transport/routes", tag = "transport",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = RouteList), (status = 503, body = Problem)))]
async fn list_routes(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<RouteList>> {
    let t = store(&a)?.tenant(org).await?;
    Ok(Json(RouteList {
        version: t.version,
        routes: t.routes,
    }))
}

/// Создаёт или заменяет политику домена назначения.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/transport/routes/{pattern}", tag = "transport",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("pattern" = String, Path, description = "`example.org`, `*.example.org` (поддомены) или `*` (все внешние)")),
    request_body = RouteBody,
    responses((status = 200, body = Written), (status = 400, body = Problem),
        (status = 409, description = "устаревшая версия", body = Problem), (status = 503, body = Problem)))]
async fn put_route(
    State(a): State<Arc<Admin>>,
    Path((org, p)): Path<(TenantId, String)>,
    Json(body): Json<RouteBody>,
) -> ApiResult<Json<Written>> {
    let t = store(&a)?;
    let policy = DestinationPolicy {
        pattern: pattern(&p)?,
        tls: body.tls,
        smart_host: body.smart_host,
        egress_ip: body.egress_ip,
    };
    let version = t.put_route(org, body.version, &policy).await?;
    Ok(Json(Written { version }))
}

/// Удаляет политику домена назначения. Нет — 404.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/transport/routes/{pattern}", tag = "transport",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("pattern" = String, Path, description = "Шаблон"), VersionQuery),
    responses((status = 200, body = Written), (status = 404, body = Problem),
        (status = 409, description = "устаревшая версия", body = Problem), (status = 503, body = Problem)))]
async fn delete_route(
    State(a): State<Arc<Admin>>,
    Path((org, p)): Path<(TenantId, String)>,
    Query(q): Query<VersionQuery>,
) -> ApiResult<Json<Written>> {
    let t = store(&a)?;
    let version = t.delete_route(org, q.version, &pattern(&p)?).await?;
    Ok(Json(Written { version }))
}

/// Снимок уровня кластера: домены всех организаций, все источники.
#[utoipa::path(get, path = "/api/v1/transport", tag = "transport",
    responses((status = 200, body = ClusterView), (status = 403, body = Problem), (status = 503, body = Problem)))]
async fn cluster(State(a): State<Arc<Admin>>) -> ApiResult<Json<ClusterView>> {
    Ok(Json(store(&a)?.cluster().await?.into()))
}

/// Создаёт или заменяет источник кластера (общий шлюз; источник приложения — только у организации, 400).
#[utoipa::path(put, path = "/api/v1/transport/sources/{name}", tag = "transport",
    params(("name" = String, Path, description = "Имя источника ([a-z0-9._-], до 64 символов)")),
    request_body = SourceBody,
    responses((status = 200, body = Written), (status = 400, body = Problem), (status = 403, body = Problem),
        (status = 409, description = "устаревшая версия или имя занято", body = Problem),
        (status = 503, body = Problem)))]
async fn put_cluster_source(
    State(a): State<Arc<Admin>>,
    Path(name): Path<String>,
    Json(body): Json<SourceBody>,
) -> ApiResult<Json<Written>> {
    put_source(&a, None, name, body).await
}

/// Удаляет источник кластера. Нет — 404.
#[utoipa::path(delete, path = "/api/v1/transport/sources/{name}", tag = "transport",
    params(("name" = String, Path, description = "Имя источника"), VersionQuery),
    responses((status = 200, body = Written), (status = 403, body = Problem), (status = 404, body = Problem),
        (status = 409, description = "устаревшая версия", body = Problem), (status = 503, body = Problem)))]
async fn delete_cluster_source(
    State(a): State<Arc<Admin>>,
    Path(name): Path<String>,
    Query(q): Query<VersionQuery>,
) -> ApiResult<Json<Written>> {
    delete_source(&a, None, &name, q.version).await
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use tp_contracts::TenantId;
    use tp_contracts::transport::TransportStore;

    use crate::testenv::Env;

    fn org() -> String {
        format!("/api/v1/orgs/{}/transport", TenantId::DEFAULT)
    }

    fn version(b: &Value) -> u64 {
        b["version"].as_u64().expect("version")
    }

    /// Приёмка ADM-08: принятые домены — создание, замена вида, удаление; устаревшая версия — 409.
    #[tokio::test]
    async fn domains() {
        let env = Env::new();
        let base = format!("{}/domains", org());
        let (s, _, b) = env.call("GET", &base, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b, json!({"version": 0, "domains": []}));

        let body = json!({"version": 0, "name": "Example.ORG."});
        let (s, _, b) = env.call("POST", &base, Some("org"), Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        assert_eq!(b["domain"]["name"], "example.org");
        assert_eq!(b["domain"]["kind"], "authoritative");
        let v1 = version(&b);
        let id = b["domain"]["id"].as_str().expect("id").to_owned();

        let stale = json!({"version": 0, "name": "other.org"});
        let (s, _, _) = env.call("POST", &base, Some("org"), Some(stale)).await;
        assert_eq!(s, StatusCode::CONFLICT, "устаревшая версия");
        let taken = json!({"version": v1, "name": "example.org"});
        let (s, _, _) = env.call("POST", &base, Some("org"), Some(taken)).await;
        assert_eq!(s, StatusCode::CONFLICT, "имя занято");
        let bad = json!({"version": v1, "name": "bad name"});
        let (s, _, _) = env.call("POST", &base, Some("org"), Some(bad)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        let relay = json!({"version": v1, "name": "example.org",
            "kind": {"internal_relay": {"next_hop": "old-mx.example.org:25"}}});
        let item = format!("{base}/{id}");
        let (s, _, b) = env
            .call("PUT", &item, Some("org"), Some(relay.clone()))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let v2 = version(&b);
        assert!(v2 > v1);
        let (s, _, _) = env.call("PUT", &item, Some("org"), Some(relay)).await;
        assert_eq!(s, StatusCode::CONFLICT, "повтор с прежней версией");
        let c = env.transport.cluster().await.expect("cluster");
        assert_eq!(c.version, v2);
        assert_eq!(
            c.domain("EXAMPLE.org").map(|d| &d.kind),
            Some(&tp_contracts::transport::DomainKind::InternalRelay {
                next_hop: "old-mx.example.org:25".into()
            })
        );

        let (s, _, _) = env
            .call("DELETE", &format!("{item}?version={v1}"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::CONFLICT, "устаревшая версия удаления");
        let (s, _, b) = env
            .call("DELETE", &format!("{item}?version={v2}"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let v3 = version(&b);
        let (s, _, _) = env
            .call("DELETE", &format!("{item}?version={v3}"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (_, _, b) = env.call("GET", &base, Some("org"), None).await;
        assert_eq!(b, json!({"version": v3, "domains": []}));
    }

    /// Политика домена — на принятом домене (решение 94): уровень «домен» в цепочке организация → домен → ящик;
    /// домен другой организации или удалённый — 404; прежнего ресурса `/orgs/{org}/domains` нет.
    #[tokio::test]
    async fn domain_policy() {
        let env = Env::new();
        let base = format!("{}/domains", org());
        let body = json!({"version": 0, "name": "example.org"});
        let (s, _, b) = env.call("POST", &base, Some("org"), Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        let v1 = version(&b);
        let id = b["domain"]["id"].as_str().expect("id").to_owned();
        let policy = format!("{base}/{id}/policy");

        let (s, _, b) = env.call("GET", &policy, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["policy"]["max_recipients"], Value::Null);
        let limit = json!({"max_recipients": 50});
        let (s, _, b) = env.call("PUT", &policy, Some("org"), Some(limit)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["max_recipients"], 50);
        let org_policy = format!("/api/v1/orgs/{}/policy", TenantId::DEFAULT);
        let up = json!({"max_recipients": 10, "send_per_hour": 100});
        let (s, _, _) = env.call("PUT", &org_policy, Some("org"), Some(up)).await;
        assert_eq!(s, StatusCode::OK);
        let (_, _, b) = env.call("GET", &policy, Some("org"), None).await;
        assert_eq!(b["policy"]["max_recipients"], 50);
        assert_eq!(b["policy"]["send_per_hour"], Value::Null);
        assert_eq!(b["effective"]["max_recipients"], 50);
        assert_eq!(b["effective"]["send_per_hour"], 100);
        let bad = json!({"trash_retention": "14 days"});
        let (s, _, _) = env.call("PUT", &policy, Some("org"), Some(bad)).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

        // Оператор — только чтение; другая организация — 403; чужой домен под своей организацией — 404.
        let (s, _, _) = env.call("GET", &policy, Some("operator"), None).await;
        assert_eq!(s, StatusCode::OK);
        let empty = || Some(json!({}));
        let (s, _, _) = env.call("PUT", &policy, Some("operator"), empty()).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env.call("GET", &policy, Some("other"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let foreign = format!(
            "/api/v1/orgs/{}/transport/domains/{id}/policy",
            env.other_tenant
        );
        let (s, _, _) = env.call("GET", &foreign, Some("other"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        let item = format!("{base}/{id}?version={v1}");
        let (s, _, _) = env.call("DELETE", &item, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _, _) = env.call("GET", &policy, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = env.call("PUT", &policy, Some("org"), empty()).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let missing = format!("{base}/{}/policy", tp_contracts::DomainId::new());
        let (s, _, _) = env.call("GET", &missing, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        // Прежние маршруты доменов организации удалены (ADM-33).
        let old = format!("/api/v1/orgs/{}/domains", TenantId::DEFAULT);
        for (method, path, body) in [
            ("GET", old.clone(), None),
            ("POST", old.clone(), Some(json!({"name": "a.example"}))),
            ("GET", format!("{old}/{id}"), None),
            ("DELETE", format!("{old}/{id}"), None),
            ("PUT", format!("{old}/{id}/policy"), empty()),
        ] {
            let (s, _, _) = env.call(method, &path, Some("org"), body).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{method} {path}");
        }
    }

    /// Домены и источники другой организации не видны и не меняются.
    #[tokio::test]
    async fn tenant_isolation() {
        let env = Env::new();
        let other = format!("/api/v1/orgs/{}/transport", env.other_tenant);
        let body = json!({"version": 0, "name": "other.org"});
        let (s, _, b) = env
            .call(
                "POST",
                &format!("{other}/domains"),
                Some("root"),
                Some(body),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        let id = b["domain"]["id"].as_str().expect("id").to_owned();
        let v = version(&b);
        let src = json!({"version": v, "matcher": {"networks": ["192.0.2.0/24"]},
            "kind": {"application": {"senders": ["@other.org"], "external": false}}});
        let (s, _, b) = env
            .call(
                "PUT",
                &format!("{other}/sources/mfp"),
                Some("root"),
                Some(src),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let v = version(&b);

        let (_, _, b) = env
            .call("GET", &format!("{}/domains", org()), Some("org"), None)
            .await;
        assert_eq!(b, json!({"version": v, "domains": []}));
        let (_, _, b) = env
            .call("GET", &format!("{}/sources", org()), Some("org"), None)
            .await;
        assert_eq!(b, json!({"version": v, "sources": []}));
        let put = json!({"version": v, "name": "mine.org"});
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{}/domains/{id}", org()),
                Some("org"),
                Some(put),
            )
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "чужой домен");
        let (s, _, _) = env
            .call(
                "DELETE",
                &format!("{}/domains/{id}?version={v}", org()),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = env
            .call(
                "DELETE",
                &format!("{}/sources/mfp?version={v}", org()),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "чужой источник");
        let src = json!({"version": v, "matcher": {"networks": ["198.51.100.0/24"]},
            "kind": {"gateway": {"xclient": true, "authserv_id": null}}});
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{}/sources/mfp", org()),
                Some("org"),
                Some(src),
            )
            .await;
        assert_eq!(s, StatusCode::CONFLICT, "имя занято другой организацией");
        let (s, _, _) = env
            .call("GET", &format!("{other}/domains"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    /// Источники организации и кластера; источники кластера — только администратор инсталляции.
    #[tokio::test]
    async fn sources() {
        let env = Env::new();
        let gw = json!({"version": 0, "matcher": {"networks": ["192.0.2.0/24"]},
            "kind": {"gateway": {"xclient": true, "authserv_id": "gw.example.org"}}});
        for token in ["org", "operator", "auditor"] {
            let (s, _, _) = env
                .call(
                    "PUT",
                    "/api/v1/transport/sources/gw",
                    Some(token),
                    Some(gw.clone()),
                )
                .await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{token}");
            let (s, _, _) = env
                .call("GET", "/api/v1/transport", Some(token), None)
                .await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{token}");
        }
        let (s, _, b) = env
            .call(
                "PUT",
                "/api/v1/transport/sources/gw",
                Some("root"),
                Some(gw),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let v1 = version(&b);
        let (_, _, b) = env
            .call("GET", "/api/v1/transport", Some("root"), None)
            .await;
        assert_eq!(b["version"], v1);
        assert_eq!(b["sources"][0]["tenant"], Value::Null);

        let app = json!({"version": v1, "matcher": {"client_cert": {"subject": "erp.example.org"}},
            "kind": {"application": {"senders": ["noreply@example.org"], "external": true}}});
        let (s, _, _) = env
            .call(
                "PUT",
                "/api/v1/transport/sources/erp",
                Some("root"),
                Some(app.clone()),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "приложение без организации");
        let base = format!("{}/sources", org());
        let (s, _, _) = env
            .call("PUT", &format!("{base}/gw"), Some("org"), Some(app.clone()))
            .await;
        assert_eq!(s, StatusCode::CONFLICT, "имя занято источником кластера");
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{base}/Bad%20Name"),
                Some("org"),
                Some(app.clone()),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, b) = env
            .call(
                "PUT",
                &format!("{base}/erp"),
                Some("org"),
                Some(app.clone()),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let v2 = version(&b);
        let (s, _, _) = env
            .call("PUT", &format!("{base}/erp"), Some("org"), Some(app))
            .await;
        assert_eq!(s, StatusCode::CONFLICT, "устаревшая версия");

        let (s, _, b) = env.call("GET", &base, Some("operator"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["version"], v2);
        assert_eq!(b["sources"].as_array().map(Vec::len), Some(1), "{b}");
        assert_eq!(b["sources"][0]["name"], "erp");
        let (s, _, _) = env
            .call(
                "DELETE",
                &format!("{base}/erp?version={v2}"),
                Some("operator"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "оператор только читает");

        let (s, _, _) = env
            .call(
                "DELETE",
                &format!("{base}/gw?version={v2}"),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(
            s,
            StatusCode::NOT_FOUND,
            "источник кластера — не организации"
        );
        let (s, _, b) = env
            .call(
                "DELETE",
                &format!("{base}/erp?version={v2}"),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let v3 = version(&b);
        let (s, _, _) = env
            .call(
                "DELETE",
                &format!("/api/v1/transport/sources/gw?version={v2}"),
                Some("root"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _, _) = env
            .call(
                "DELETE",
                &format!("/api/v1/transport/sources/gw?version={v3}"),
                Some("root"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK);
        let c = env.transport.cluster().await.expect("cluster");
        assert!(c.sources.is_empty());
    }

    /// Политики доменов назначения: шаблоны, канонизация, версия организации.
    #[tokio::test]
    async fn routes() {
        let env = Env::new();
        let base = format!("{}/routes", org());
        let (s, _, b) = env.call("GET", &base, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, json!({"version": 0, "routes": []}));

        let relay = json!({"version": 0, "smart_host": "relay.example.net:587"});
        let (s, _, b) = env
            .call("PUT", &format!("{base}/*"), Some("org"), Some(relay))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let v1 = version(&b);
        let pin = "ab".repeat(32);
        let partner = json!({"version": v1, "tls": {"pinned": {"spki_sha256": [pin]}},
            "egress_ip": "203.0.113.7"});
        let (s, _, b) = env
            .call(
                "PUT",
                &format!("{base}/*.Partner.ORG"),
                Some("org"),
                Some(partner.clone()),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let v2 = version(&b);
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{base}/partner.org"),
                Some("org"),
                Some(partner),
            )
            .await;
        assert_eq!(s, StatusCode::CONFLICT, "устаревшая версия");
        let bad = json!({"version": v2, "tls": {"pinned": {"spki_sha256": ["xyz"]}}});
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{base}/partner.org"),
                Some("org"),
                Some(bad),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        let t = env
            .transport
            .tenant(TenantId::DEFAULT)
            .await
            .expect("tenant");
        assert_eq!(t.version, v2);
        let r = t.route("mx.partner.org").expect("route");
        assert_eq!(r.pattern, "*.partner.org");
        assert_eq!(
            t.route("example.com").map(|r| r.pattern.as_str()),
            Some("*")
        );
        let other = env.transport.tenant(env.other_tenant).await.expect("other");
        assert_eq!(other.version, 0, "политики — уровень организации");

        let (s, _, b) = env.call("GET", &base, Some("operator"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["routes"][1]["pattern"], "*.partner.org", "{b}");
        let (s, _, _) = env
            .call(
                "DELETE",
                &format!("{base}/nope.org?version={v2}"),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = env
            .call(
                "DELETE",
                &format!("{base}/*?version={v1}"),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _, b) = env
            .call(
                "DELETE",
                &format!("{base}/*?version={v2}"),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(version(&b) > v2);
        let n = env
            .audit
            .records()
            .iter()
            .filter(|r| r.route.contains("/transport/"))
            .count();
        assert_eq!(n, 7, "изменения — в журнале аудита");
    }

    /// Хранилище не подключено — 503; спецификация содержит маршруты.
    #[tokio::test]
    async fn unavailable_and_openapi() {
        let env = Env::without_transport();
        let (s, _, _) = env
            .call("GET", &format!("{}/routes", org()), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        let (s, _, b) = env.call("GET", "/api/openapi.json", None, None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(
            b["paths"]["/api/v1/orgs/{org}/transport/routes/{pattern}"]["put"].is_object(),
            "{b}"
        );
        assert!(b["paths"]["/api/v1/transport"]["get"].is_object());
    }
}
