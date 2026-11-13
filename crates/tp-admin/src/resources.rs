//! Ресурсы — переговорные и оборудование (SRS 4.4, 5.2; APP-I «Ящики ресурсов и бронирование»; ADM-16; CTR-48):
//! учётная запись вида `resource` (`AccountKind::Resource`) и её правила бронирования (`BookingPolicy`).
//!
//! - `GET`, `POST /api/v1/orgs/{org}/resources` — список по имени входа и создание: локальная учётная запись в
//!   принятом домене организации, как `POST …/accounts`, с правилами из запроса (нет — по умолчанию:
//!   автоматическое принятие без пересечений, до суток, на 180 дней вперёд, круглосуточно);
//! - `GET …/resources/{account}` — ресурс с правилами; `DELETE` — удаление: правила снимаются, учётная запись
//!   отключается со сроком хранения «сейчас», окончательно её удаляет обслуживание (роль maintenance) — как
//!   отключённый ящик (`…/disabled-accounts`, до удаления её можно восстановить);
//! - `PUT …/resources/{account}/booking-policy` — правила бронирования целиком.
//!
//! Имя и блокировку входа ресурса меняет `PATCH …/accounts/{account}`, представителей (ответственных за
//! бронирования) — [`crate::sharing`]. Хранилища — [`Admin::with_sharing`]; без них маршруты отвечают 503.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, put};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, Time};
use tp_contracts::config::duration;
use tp_contracts::meta::{Account, AccountKind, AccountScope, AccountState};
use tp_contracts::sharing::{Bookers, BookingPolicy, DayOfWeek, WorkingHours};
use tp_contracts::{AccountId, Error, GroupId, Result, TenantId};
use utoipa::{OpenApi, ToSchema};

use crate::accounts::{AccountInfo, account, account_info, create_local};
use crate::extract::{Json, Path};
use crate::server::{Admin, ApiResult, Problem, Refused};
use crate::sharing::stores;

/// Кому разрешено бронировать ресурс: `{"type": "internal"}` — любой учётной записи организации,
/// `{"type": "only", "accounts": [...], "groups": [...]}` — перечисленным учётным записям и участникам групп.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
pub enum BookersBody {
    /// Любая учётная запись организации.
    #[default]
    Internal,
    /// Только перечисленные.
    Only {
        /// Учётные записи (UUID).
        #[serde(default)]
        #[schema(value_type = Vec<String>)]
        accounts: Vec<AccountId>,
        /// Группы (UUID), с вложенными.
        #[serde(default)]
        #[schema(value_type = Vec<String>)]
        groups: Vec<GroupId>,
    },
}

/// День недели.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Weekday {
    /// Понедельник.
    Monday,
    /// Вторник.
    Tuesday,
    /// Среда.
    Wednesday,
    /// Четверг.
    Thursday,
    /// Пятница.
    Friday,
    /// Суббота.
    Saturday,
    /// Воскресенье.
    Sunday,
}

impl From<DayOfWeek> for Weekday {
    fn from(d: DayOfWeek) -> Self {
        match d {
            DayOfWeek::Monday => Self::Monday,
            DayOfWeek::Tuesday => Self::Tuesday,
            DayOfWeek::Wednesday => Self::Wednesday,
            DayOfWeek::Thursday => Self::Thursday,
            DayOfWeek::Friday => Self::Friday,
            DayOfWeek::Saturday => Self::Saturday,
            DayOfWeek::Sunday => Self::Sunday,
        }
    }
}

impl From<Weekday> for DayOfWeek {
    fn from(d: Weekday) -> Self {
        match d {
            Weekday::Monday => Self::Monday,
            Weekday::Tuesday => Self::Tuesday,
            Weekday::Wednesday => Self::Wednesday,
            Weekday::Thursday => Self::Thursday,
            Weekday::Friday => Self::Friday,
            Weekday::Saturday => Self::Saturday,
            Weekday::Sunday => Self::Sunday,
        }
    }
}

/// Рабочее время ресурса в его часовом поясе: вне его приглашения отклоняются.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkingHoursBody {
    /// Часовой пояс IANA.
    #[schema(example = "Europe/Moscow")]
    pub timezone: String,
    /// Рабочие дни (непусто).
    pub days: Vec<Weekday>,
    /// Начало рабочего дня, `ЧЧ:ММ`.
    #[schema(example = "09:00")]
    pub from: String,
    /// Конец рабочего дня, `ЧЧ:ММ`, позже начала.
    #[schema(example = "18:00")]
    pub to: String,
}

/// Правила бронирования ресурса; отсутствующее поле — значение по умолчанию.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BookingPolicyBody {
    /// Подходящее приглашение принимается автоматически; иначе остаётся предварительным до решения представителя
    /// ресурса.
    pub auto_accept: bool,
    /// Разрешены пересекающиеся бронирования.
    pub allow_conflicts: bool,
    /// Наибольшая длительность встречи (`4h`); `null` — без ограничения.
    #[serde(with = "duration::option")]
    #[schema(value_type = Option<String>, example = "4h", default = "1d")]
    pub max_duration: Option<Duration>,
    /// Насколько вперёд можно бронировать (`180d`); `null` — без ограничения.
    #[serde(with = "duration::option")]
    #[schema(value_type = Option<String>, example = "90d", default = "180d")]
    pub horizon: Option<Duration>,
    /// Кому разрешено бронировать.
    pub bookers: BookersBody,
    /// Рабочее время; `null` — круглосуточно.
    pub working_hours: Option<WorkingHoursBody>,
}

impl Default for BookingPolicyBody {
    fn default() -> Self {
        BookingPolicy::default().into()
    }
}

/// Время `ЧЧ:ММ`.
fn hhmm(t: Time) -> String {
    format!("{:02}:{:02}", t.hour(), t.minute())
}

/// Разбор `ЧЧ:ММ`; иначе — `Invalid`.
fn parse_hhmm(s: &str) -> Result<Time> {
    let bad = || Error::invalid(format!("время {s:?} — не ЧЧ:ММ"));
    let (h, m) = s.split_once(':').ok_or_else(bad)?;
    if h.len() != 2 || m.len() != 2 {
        return Err(bad());
    }
    let h: u8 = h.parse().map_err(|_| bad())?;
    let m: u8 = m.parse().map_err(|_| bad())?;
    Time::from_hms(h, m, 0).map_err(|_| bad())
}

impl From<BookingPolicy> for BookingPolicyBody {
    fn from(p: BookingPolicy) -> Self {
        Self {
            auto_accept: p.auto_accept,
            allow_conflicts: p.allow_conflicts,
            max_duration: p.max_duration,
            horizon: p.horizon,
            bookers: match p.bookers {
                Bookers::Internal => BookersBody::Internal,
                Bookers::Only { accounts, groups } => BookersBody::Only { accounts, groups },
            },
            working_hours: p.working_hours.map(|w| WorkingHoursBody {
                timezone: w.timezone,
                days: w.days.into_iter().map(Weekday::from).collect(),
                from: hhmm(w.from),
                to: hhmm(w.to),
            }),
        }
    }
}

impl TryFrom<BookingPolicyBody> for BookingPolicy {
    type Error = Error;

    /// Правила по телу запроса с проверкой [`BookingPolicy::validate`]; иначе — `Invalid`.
    fn try_from(p: BookingPolicyBody) -> Result<Self> {
        let working_hours = match p.working_hours {
            None => None,
            Some(w) => Some(WorkingHours {
                timezone: w.timezone,
                days: w
                    .days
                    .into_iter()
                    .map(DayOfWeek::from)
                    .collect::<BTreeSet<_>>(),
                from: parse_hhmm(&w.from)?,
                to: parse_hhmm(&w.to)?,
            }),
        };
        let policy = Self {
            auto_accept: p.auto_accept,
            allow_conflicts: p.allow_conflicts,
            max_duration: p.max_duration,
            horizon: p.horizon,
            bookers: match p.bookers {
                BookersBody::Internal => Bookers::Internal,
                BookersBody::Only { accounts, groups } => Bookers::Only { accounts, groups },
            },
            working_hours,
        };
        policy.validate()?;
        Ok(policy)
    }
}

/// Ресурс: учётная запись и правила бронирования.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ResourceInfo {
    /// Учётная запись ресурса.
    #[serde(flatten)]
    pub account: AccountInfo,
    /// Правила бронирования; `null` — не заданы (приглашения не обрабатываются автоматически).
    pub booking_policy: Option<BookingPolicyBody>,
}

/// Новый ресурс.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewResource {
    /// Имя входа — адрес в принятом домене организации (приводится к нижнему регистру).
    pub login: String,
    /// Отображаемое имя («Переговорная 5.12»); нет — имя входа.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Правила бронирования; нет — по умолчанию.
    #[serde(default)]
    pub booking_policy: Option<BookingPolicyBody>,
}

const RESOURCES: &str = "/api/v1/orgs/{org}/resources";
const RESOURCE: &str = "/api/v1/orgs/{org}/resources/{account}";
const BOOKING_POLICY: &str = "/api/v1/orgs/{org}/resources/{account}/booking-policy";

/// Маршруты ресурсов.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route(RESOURCES, get(list_resources).post(create_resource))
        .route(RESOURCE, get(get_resource).delete(delete_resource))
        .route(BOOKING_POLICY, put(put_booking_policy))
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(
        list_resources,
        create_resource,
        get_resource,
        delete_resource,
        put_booking_policy
    ),
    components(schemas(
        BookersBody,
        Weekday,
        WorkingHoursBody,
        BookingPolicyBody,
        ResourceInfo,
        NewResource
    ))
)]
pub(crate) struct Doc;

/// Правила по телу запроса: проверка полей и ссылок на учётные записи и группы организации.
async fn policy_of(a: &Admin, org: TenantId, body: BookingPolicyBody) -> Result<BookingPolicy> {
    let policy = BookingPolicy::try_from(body)?;
    if let Bookers::Only { accounts, groups } = &policy.bookers {
        crate::groups::check_ids(a, org, accounts.clone(), groups.clone()).await?;
    }
    Ok(policy)
}

/// Учётная запись-ресурс организации; нет, не ресурс или удалена (отключена) — `NotFound`.
async fn resource(a: &Admin, org: TenantId, id: AccountId) -> Result<Account> {
    let acc = account(a, org, id).await?;
    if acc.kind != AccountKind::Resource || !active(a, &acc).await? {
        return Err(Error::NotFound);
    }
    Ok(acc)
}

/// Учётная запись действует (не отключена и не удаляется).
async fn active(a: &Admin, acc: &Account) -> Result<bool> {
    Ok(a.d.accounts.account_state(acc.tenant, acc.id).await? == AccountState::Active)
}

async fn resource_info(a: &Admin, acc: Account) -> Result<ResourceInfo> {
    let s = stores(a)?;
    let scope = AccountScope {
        tenant: acc.tenant,
        account: acc.id,
    };
    let booking_policy = s.resources.booking_policy(scope).await?.map(Into::into);
    Ok(ResourceInfo {
        account: account_info(a, acc).await?,
        booking_policy,
    })
}

/// Действующие ресурсы организации по имени входа (удалённые, но ещё хранимые — среди отключённых ящиков).
#[utoipa::path(get, path = "/api/v1/orgs/{org}/resources", tag = "resources",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = Vec<ResourceInfo>), (status = 403, body = Problem), (status = 503, body = Problem)))]
async fn list_resources(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<Vec<ResourceInfo>>> {
    let s = stores(&a)?;
    let ids = s.resources.resources(org).await?;
    let mut accounts = a.d.accounts.by_ids(org, &ids).await?;
    accounts.sort_by(|x, y| x.login.cmp(&y.login));
    let mut out = Vec::with_capacity(accounts.len());
    for acc in accounts {
        if active(&a, &acc).await? {
            out.push(resource_info(&a, acc).await?);
        }
    }
    Ok(Json(out))
}

/// Создаёт ресурс: локальную учётную запись вида `resource` в принятом домене организации и правила бронирования.
/// Имя занято — 409; домен не принят организацией, недопустимые правила или неизвестная учётная запись или группа в
/// `bookers` — 400; хранилища не подключены — 503.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/resources", tag = "resources",
    params(("org" = String, Path, description = "Организация (UUID)")),
    request_body = NewResource,
    responses((status = 201, body = ResourceInfo), (status = 400, body = Problem), (status = 409, body = Problem),
        (status = 503, body = Problem)))]
async fn create_resource(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Json(body): Json<NewResource>,
) -> ApiResult<(StatusCode, Json<ResourceInfo>)> {
    let s = stores(&a)?;
    let policy = policy_of(&a, org, body.booking_policy.unwrap_or_default()).await?;
    let acc = create_local(
        &a,
        org,
        &body.login,
        body.display_name.as_deref(),
        AccountKind::Resource,
    )
    .await?;
    let scope = AccountScope {
        tenant: org,
        account: acc.id,
    };
    s.resources.set_booking_policy(scope, Some(policy)).await?;
    Ok((StatusCode::CREATED, Json(resource_info(&a, acc).await?)))
}

/// Ресурс и его правила бронирования; нет или не ресурс — 404.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/resources/{account}", tag = "resources",
    params(("org" = String, Path, description = "Организация (UUID)"), ("account" = String, Path, description = "Ресурс (UUID)")),
    responses((status = 200, body = ResourceInfo), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn get_resource(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<Json<ResourceInfo>> {
    stores(&a)?;
    let acc = resource(&a, org, id).await?;
    Ok(Json(resource_info(&a, acc).await?))
}

/// Задаёт правила бронирования ресурса целиком. Недопустимые правила или неизвестная учётная запись или группа в
/// `bookers` — 400; нет или не ресурс — 404.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/resources/{account}/booking-policy", tag = "resources",
    params(("org" = String, Path, description = "Организация (UUID)"), ("account" = String, Path, description = "Ресурс (UUID)")),
    request_body = BookingPolicyBody,
    responses((status = 200, body = BookingPolicyBody), (status = 400, body = Problem), (status = 404, body = Problem),
        (status = 503, body = Problem)))]
async fn put_booking_policy(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
    Json(body): Json<BookingPolicyBody>,
) -> ApiResult<Json<BookingPolicyBody>> {
    let s = stores(&a)?;
    resource(&a, org, id).await?;
    let policy = policy_of(&a, org, body).await?;
    let scope = AccountScope {
        tenant: org,
        account: id,
    };
    s.resources
        .set_booking_policy(scope, Some(policy.clone()))
        .await?;
    Ok(Json(policy.into()))
}

/// Удаляет ресурс: снимает правила и отключает учётную запись со сроком хранения «сейчас» — окончательно её удаляет
/// обслуживание. Ресурс из каталога — 409 (удаляется в каталоге); нет или не ресурс — 404.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/resources/{account}", tag = "resources",
    params(("org" = String, Path, description = "Организация (UUID)"), ("account" = String, Path, description = "Ресурс (UUID)")),
    responses((status = 204), (status = 404, body = Problem), (status = 409, body = Problem), (status = 503, body = Problem)))]
async fn delete_resource(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> std::result::Result<StatusCode, Refused> {
    let s = stores(&a)?;
    let acc = resource(&a, org, id).await?;
    if acc.external_id.is_some() {
        return Err(Refused::Problem(
            StatusCode::CONFLICT,
            "ресурс из каталога удаляется в каталоге".to_owned(),
        ));
    }
    let scope = AccountScope {
        tenant: org,
        account: id,
    };
    s.resources.set_booking_policy(scope, None).await?;
    a.d.accounts
        .disable_account(org, id, OffsetDateTime::now_utc())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tp_contracts::meta::AccountStore;
    use tp_contracts::sharing::ResourceStore;
    use tp_contracts::transport::{AcceptedDomain, DomainKind, TransportStore};

    use super::*;
    use crate::testenv::Env;

    fn base() -> String {
        format!("/api/v1/orgs/{}/resources", TenantId::DEFAULT)
    }

    async fn accept_domain(env: &Env) {
        let c = env.transport.cluster().await.expect("cluster");
        let d = AcceptedDomain {
            id: tp_contracts::DomainId::new(),
            tenant: TenantId::DEFAULT,
            name: "example.test".into(),
            kind: DomainKind::Authoritative,
        };
        env.transport
            .put_domain(c.version, &d)
            .await
            .expect("domain");
    }

    /// Приёмка ADM-16: создание ресурса с правилами по умолчанию и заданными, список, чтение, ошибки.
    #[tokio::test]
    async fn create_and_list() {
        let env = Env::new();
        accept_domain(&env).await;
        let (s, _, b) = env
            .call(
                "POST",
                &base(),
                Some("org"),
                Some(
                    json!({"login": "Room-512@example.test", "display_name": "Переговорная 5.12"}),
                ),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        assert_eq!(b["login"], "room-512@example.test");
        assert_eq!(b["display_name"], "Переговорная 5.12");
        assert_eq!(
            b["booking_policy"],
            json!({"auto_accept": true, "allow_conflicts": false, "max_duration": "1d", "horizon": "180d",
                   "bookers": {"type": "internal"}, "working_hours": null})
        );
        let room = b["id"].as_str().expect("id").to_owned();

        let staff = env.account("boss@example.org").await;
        let policy = json!({
            "auto_accept": false, "max_duration": "4h", "horizon": null,
            "bookers": {"type": "only", "accounts": [staff.account]},
            "working_hours": {"timezone": "Europe/Moscow", "days": ["monday", "friday"], "from": "09:00", "to": "18:30"},
        });
        let (s, _, b) = env
            .call(
                "POST",
                &base(),
                Some("org"),
                Some(json!({"login": "projector@example.test", "booking_policy": policy})),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        let p = &b["booking_policy"];
        assert_eq!(p["auto_accept"], false);
        assert_eq!(p["max_duration"], "4h");
        assert_eq!(p["horizon"], Value::Null);
        assert_eq!(p["bookers"]["accounts"], json!([staff.account]));
        assert_eq!(p["working_hours"]["days"], json!(["monday", "friday"]));
        assert_eq!(p["working_hours"]["to"], "18:30");

        // Учётная запись — вида resource.
        let id: AccountId = room.parse().expect("uuid");
        let acc = env
            .meta
            .get(TenantId::DEFAULT, id)
            .await
            .expect("get")
            .expect("account");
        assert_eq!(acc.kind, AccountKind::Resource);

        let (s, _, b) = env.call("GET", &base(), Some("operator"), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let logins: Vec<&str> = b
            .as_array()
            .expect("list")
            .iter()
            .filter_map(|r| r["login"].as_str())
            .collect();
        assert_eq!(logins, ["projector@example.test", "room-512@example.test"]);
        let (s, _, b) = env
            .call("GET", &format!("{}/{room}", base()), Some("org"), None)
            .await;
        assert_eq!(
            (s, &b["login"]),
            (StatusCode::OK, &json!("room-512@example.test"))
        );
        // Пользователь — не ресурс.
        let (s, _, _) = env
            .call(
                "GET",
                &format!("{}/{}", base(), staff.account),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        // Имя занято — 409; оператор — 403; недопустимые правила — 400, учётная запись не создаётся.
        let (s, _, _) = env
            .call(
                "POST",
                &base(),
                Some("org"),
                Some(json!({"login": "room-512@example.test"})),
            )
            .await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _, _) = env
            .call(
                "POST",
                &base(),
                Some("operator"),
                Some(json!({"login": "x@example.test"})),
            )
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        for bad in [
            json!({"max_duration": "0s"}),
            json!({"working_hours": {"timezone": "UTC", "days": [], "from": "09:00", "to": "18:00"}}),
            json!({"working_hours": {"timezone": "UTC", "days": ["monday"], "from": "18:00", "to": "09:00"}}),
            json!({"working_hours": {"timezone": "UTC", "days": ["monday"], "from": "9", "to": "18:00"}}),
            json!({"bookers": {"type": "only", "groups": [GroupId::new()]}}),
        ] {
            let (s, _, b) = env
                .call(
                    "POST",
                    &base(),
                    Some("org"),
                    Some(json!({"login": "bad@example.test", "booking_policy": bad})),
                )
                .await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        }
        assert!(
            env.meta
                .by_login(TenantId::DEFAULT, "bad@example.test")
                .await
                .expect("by_login")
                .is_none()
        );
        assert!(
            env.audit
                .records()
                .iter()
                .any(|r| r.route == RESOURCES && r.status == 201)
        );
    }

    /// Приёмка ADM-16: изменение правил и удаление ресурса.
    #[tokio::test]
    async fn change_and_delete() {
        let env = Env::new();
        accept_domain(&env).await;
        let (_, _, b) = env
            .call(
                "POST",
                &base(),
                Some("org"),
                Some(json!({"login": "room@example.test"})),
            )
            .await;
        let room = b["id"].as_str().expect("id").to_owned();
        let url = format!("{}/{room}/booking-policy", base());
        let (s, _, b) = env
            .call(
                "PUT",
                &url,
                Some("org"),
                Some(json!({"allow_conflicts": true, "max_duration": null})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["allow_conflicts"], true);
        assert_eq!(b["max_duration"], Value::Null);
        assert_eq!(b["horizon"], "180d", "отсутствующее поле — по умолчанию");
        let (_, _, b) = env
            .call("GET", &format!("{}/{room}", base()), Some("org"), None)
            .await;
        assert_eq!(b["booking_policy"]["allow_conflicts"], true);
        let (s, _, _) = env
            .call("PUT", &url, Some("org"), Some(json!({"unknown": 1})))
            .await;
        assert!(s.is_client_error());
        let user = env.account("user@example.org").await;
        let (s, _, _) = env
            .call(
                "PUT",
                &format!("{}/{}/booking-policy", base(), user.account),
                Some("org"),
                Some(json!({})),
            )
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        let one = format!("{}/{room}", base());
        let (s, _, _) = env.call("DELETE", &one, Some("operator"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, b) = env.call("DELETE", &one, Some("org"), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
        let id: AccountId = room.parse().expect("uuid");
        let state = env
            .meta
            .account_state(TenantId::DEFAULT, id)
            .await
            .expect("state");
        assert!(matches!(state, AccountState::Disabled { .. }), "{state:?}");
        let scope = AccountScope {
            tenant: TenantId::DEFAULT,
            account: id,
        };
        assert_eq!(
            env.meta.booking_policy(scope).await.expect("booking"),
            None,
            "правила сняты"
        );
        let (s, _, _) = env
            .call(
                "DELETE",
                &format!("{}/{}", base(), user.account),
                Some("org"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        // Удалённый — нет ни в списке, ни по идентификатору.
        let (_, _, b) = env.call("GET", &base(), Some("org"), None).await;
        assert_eq!(b, json!([]));
        let (s, _, _) = env.call("DELETE", &one, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[test]
    fn booking_policy_body_roundtrip() {
        let body = BookingPolicyBody::default();
        let policy = BookingPolicy::try_from(body.clone()).expect("policy");
        assert_eq!(policy, BookingPolicy::default());
        assert_eq!(BookingPolicyBody::from(policy), body);
        for bad in ["24:00", "9:00", "09:60", "09-00", "ab:cd"] {
            assert!(parse_hhmm(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_hhmm("23:59").map(hhmm).ok().as_deref(), Some("23:59"));
    }
}
