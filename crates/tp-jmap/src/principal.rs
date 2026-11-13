//! JMAP — участники организации и занятость (JMAP-30): `Principal/get`, `/query` (RFC 9670 §2) и
//! `Principal/getAvailability` (draft-ietf-jmap-calendars) поверх [`AccountStore`] и [`FreeBusy`].
//!
//! Участник — действующая учётная запись организации с разрешённым входом: `id` — её идентификатор, `email` — имя
//! входа, `name` — отображаемое имя. Список только для чтения, состояние постоянно (`/changes`, `/set`,
//! `/queryChanges` нет; `canCalculateChanges` — `false`).
//!
//! Отличия от RFC 9670 и черновика:
//! - `type` — `individual` или `resource` по виду учётной записи (`Account::kind`, CTR-48); фильтр `type` с другим
//!   значением ничего не находит;
//! - `description`, `timeZone`, `accounts` — `null`;
//! - занятость видна всем участникам организации (права «только занятость» и политика — CTR-48);
//!   `showDetails` не поддерживается: `event` периода — `null`;
//! - занятость — по индексу [`tp_contracts::dav::ObjectIndex::busy`]; периоды за его горизонтом могут отсутствовать.
//!
//! Источник занятости [`busy`] — общий с `Availability/get` (jmap-09).

use std::cmp::Ordering;
use std::str::FromStr;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tp_contracts::ids::{AccountId, TenantId};
use tp_contracts::meta::{Account, AccountKind, AccountState, AccountStore};
use tp_contracts::schedule::{BusyKind, BusyPeriod, Period};

use crate::core::error::{MethodError, MethodResult};
use crate::core::request::{
    Comparator, GetArgs, QueryWindow, apply_window, object, parse_args, select,
};
use crate::core::{CALENDARS, Ctx, Jmap};
use crate::email_get::utc_date;

/// Состояние участников: список только для чтения.
const STATE: &str = "0";

/// Учётных записей за одно обращение к хранилищу.
const PAGE: usize = 500;

/// Значение capability `principals` учётной записи.
pub(crate) fn account_capability(account: &str) -> Value {
    json!({ "currentUserPrincipalId": account })
}

/// Значение capability `principals:owner` учётной записи.
pub(crate) fn owner_capability(account: &str) -> Value {
    json!({ "accountIdForPrincipal": account, "principalId": account })
}

/// Значение capability `principals:availability` учётной записи.
pub(crate) fn availability_capability(jmap: &Jmap) -> Value {
    let max = jmap.cfg.calendars.max_availability_duration.as_secs();
    json!({ "maxAvailabilityDuration": format!("PT{max}S") })
}

/// Учётная запись видна как участник: вход разрешён и ящик действует.
async fn visible(store: &dyn AccountStore, a: &Account) -> MethodResult<bool> {
    if !a.enabled {
        return Ok(false);
    }
    Ok(store.account_state(a.tenant, a.id).await? == AccountState::Active)
}

/// Участники организации по имени входа.
async fn all(jmap: &Jmap, tenant: TenantId) -> MethodResult<Vec<Account>> {
    let store = jmap.d.accounts.as_ref();
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = store.list(tenant, "", after.as_deref(), PAGE).await?;
        let full = page.len() >= PAGE;
        after = page.last().map(|a| a.login.to_lowercase());
        for a in page {
            if visible(store, &a).await? {
                out.push(a);
            }
        }
        if !full {
            return Ok(out);
        }
    }
}

/// Участник по `id`; нет, отключён или чужой организации — `None`.
pub(crate) async fn find(jmap: &Jmap, tenant: TenantId, id: &str) -> MethodResult<Option<Account>> {
    let Ok(id) = AccountId::from_str(id) else {
        return Ok(None);
    };
    let store = jmap.d.accounts.as_ref();
    let Some(a) = store.get(tenant, id).await? else {
        return Ok(None);
    };
    Ok(visible(store, &a).await?.then_some(a))
}

/// `Principal.type` (RFC 9670) по виду учётной записи.
fn principal_type(kind: AccountKind) -> &'static str {
    match kind {
        AccountKind::User => "individual",
        AccountKind::Resource => "resource",
    }
}

fn principal_json(ctx: &Ctx<'_>, a: &Account) -> Map<String, Value> {
    let id = a.id.to_string();
    let own = a.id == ctx.scope.account;
    let mut caps = Map::new();
    if ctx.jmap.dav.is_some() {
        caps.insert(
            CALENDARS.into(),
            json!({
                "accountId": own.then_some(&id),
                "mayGetAvailability": ctx.jmap.freebusy.is_some(),
                "mayShareWith": ctx.jmap.sharing.is_some(),
                "calendarAddress": format!("mailto:{}", a.login),
            }),
        );
    }
    object(json!({
        "id": id,
        "type": principal_type(a.kind),
        "name": a.display_name,
        "description": null,
        "email": a.login,
        "timeZone": null,
        "capabilities": caps,
        "accounts": null,
    }))
}

/// `Principal/get`.
pub(crate) async fn get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let tenant = ctx.scope.tenant;
    let props = a.properties.as_deref();
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    match &a.ids {
        None => {
            let accounts = all(ctx.jmap, tenant).await?;
            list.extend(
                accounts
                    .iter()
                    .map(|p| select(principal_json(ctx, p), props)),
            );
        }
        Some(ids) => {
            let mut seen: Vec<&str> = Vec::new();
            for id in ids {
                let id = ctx.resolve_id(id);
                if seen.contains(&id) {
                    continue;
                }
                seen.push(id);
                match find(ctx.jmap, tenant, id).await? {
                    Some(p) => list.push(select(principal_json(ctx, &p), props)),
                    None => not_found.push(id.to_owned()),
                }
            }
        }
    }
    Ok(object(json!({
        "accountId": a.account_id,
        "state": STATE,
        "list": list,
        "notFound": not_found,
    })))
}

/// Проверка фильтра (RFC 9670 §2.4: `accountIds`, `email`, `name`, `text`, `type`, `timeZone`; операторы).
fn matches(filter: &Value, own: AccountId, a: &Account) -> MethodResult<bool> {
    let Value::Object(f) = filter else {
        return Err(MethodError::invalid("filter must be an object"));
    };
    if let Some(op) = f.get("operator") {
        let conds = f
            .get("conditions")
            .and_then(Value::as_array)
            .ok_or_else(|| MethodError::invalid("conditions must be an array"))?;
        let mut results = Vec::with_capacity(conds.len());
        for c in conds {
            results.push(matches(c, own, a)?);
        }
        return match op.as_str() {
            Some("AND") => Ok(results.iter().all(|r| *r)),
            Some("OR") => Ok(results.iter().any(|r| *r)),
            Some("NOT") => Ok(!results.iter().any(|r| *r)),
            _ => Err(MethodError::invalid("unknown filter operator")),
        };
    }
    let (name, email) = (a.display_name.to_lowercase(), a.login.to_lowercase());
    let mut ok = true;
    for (key, val) in f {
        let unsupported = || MethodError::with("unsupportedFilter", key.clone());
        let text = || val.as_str().map(str::to_lowercase).ok_or_else(unsupported);
        ok &= match key.as_str() {
            // Учётная запись участника доступна только ему самому.
            "accountIds" => {
                let own = own.to_string();
                let ids = val.as_array().ok_or_else(unsupported)?;
                a.id.to_string() == own && ids.iter().any(|i| i.as_str() == Some(&own))
            }
            "email" => email.contains(&text()?),
            "name" => name.contains(&text()?),
            "text" => {
                let t = text()?;
                name.contains(&t) || email.contains(&t)
            }
            "type" => val.as_str().ok_or_else(unsupported)? == principal_type(a.kind),
            "timeZone" => {
                val.as_str().ok_or_else(unsupported)?;
                false
            }
            _ => return Err(unsupported()),
        };
    }
    Ok(ok)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryArgs {
    account_id: String,
    filter: Option<Value>,
    sort: Option<Vec<Comparator>>,
    #[serde(flatten)]
    window: QueryWindow,
}

/// `Principal/query`: сортировка — `name`, `email` (по умолчанию — имя входа).
pub(crate) async fn query(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: QueryArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let sort = a.sort.as_deref().unwrap_or_default();
    if sort
        .iter()
        .any(|c| !matches!(c.property.as_str(), "name" | "email"))
    {
        return Err(MethodError::new("unsupportedSort"));
    }
    let filter = a.filter.as_ref().filter(|f| !f.is_null());
    let mut found = Vec::new();
    for p in all(ctx.jmap, ctx.scope.tenant).await? {
        if match filter {
            Some(f) => matches(f, ctx.scope.account, &p)?,
            None => true,
        } {
            found.push(p);
        }
    }
    let key = |p: &Account, prop: &str| match prop {
        "name" => p.display_name.to_lowercase(),
        _ => p.login.to_lowercase(),
    };
    found.sort_by(|x, y| {
        sort.iter()
            .map(|c| {
                let o = key(x, &c.property).cmp(&key(y, &c.property));
                if c.is_ascending { o } else { o.reverse() }
            })
            .find(|o| *o != Ordering::Equal)
            .unwrap_or_else(|| key(x, "email").cmp(&key(y, "email")))
    });
    let ids: Vec<String> = found.iter().map(|p| p.id.to_string()).collect();
    let mut out = object(json!({
        "accountId": a.account_id,
        "queryState": STATE,
        "canCalculateChanges": false,
    }));
    apply_window(&ids, &a.window, ctx.jmap.cfg.max_query_limit, &mut out)?;
    Ok(out)
}

/// Занятость учётных записей организации в интервале — источник `Principal/getAvailability` и
/// `Availability/get`; хранилище занятости не подключено — `unknownMethod`.
pub(crate) async fn busy(
    jmap: &Jmap,
    tenant: TenantId,
    accounts: &[AccountId],
    range: Period,
) -> MethodResult<Vec<(AccountId, Vec<BusyPeriod>)>> {
    let fb = jmap
        .freebusy
        .as_ref()
        .ok_or(MethodError::new("unknownMethod"))?;
    Ok(fb.query(tenant, accounts, range).await?)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AvailabilityArgs {
    account_id: String,
    id: String,
    utc_start: String,
    utc_end: String,
    #[serde(default)]
    show_details: bool,
    event_properties: Option<Vec<String>>,
}

fn status(kind: BusyKind) -> &'static str {
    match kind {
        BusyKind::Busy => "confirmed",
        BusyKind::Tentative => "tentative",
        BusyKind::Unavailable => "unavailable",
    }
}

/// `Principal/getAvailability`: периоды занятости участника `id` в интервале `utcStart`…`utcEnd`.
pub(crate) async fn get_availability(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: AvailabilityArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    if a.show_details || a.event_properties.is_some() {
        return Err(MethodError::invalid(
            "showDetails and eventProperties are not supported",
        ));
    }
    let at = |s: &str, name: &str| {
        OffsetDateTime::parse(s, &Rfc3339)
            .map_err(|_| MethodError::invalid(format!("{name} must be a UTCDate")))
    };
    let range = Period {
        start: at(&a.utc_start, "utcStart")?,
        end: at(&a.utc_end, "utcEnd")?,
    };
    if range.validate().is_err() {
        return Err(MethodError::invalid("utcEnd must be after utcStart"));
    }
    let max = ctx.jmap.cfg.calendars.max_availability_duration;
    if range.end - range.start > max {
        return Err(MethodError::invalid(
            "the time range is longer than maxAvailabilityDuration",
        ));
    }
    let tenant = ctx.scope.tenant;
    let id = ctx.resolve_id(&a.id).to_owned();
    let Some(p) = find(ctx.jmap, tenant, &id).await? else {
        return Err(MethodError::with("notFound", "principal not found"));
    };
    let periods = busy(ctx.jmap, tenant, &[p.id], range).await?;
    let list: Vec<Value> = periods
        .into_iter()
        .flat_map(|(_, list)| list)
        .map(|b| {
            json!({
                "utcStart": utc_date(b.start),
                "utcEnd": utc_date(b.end),
                "busyStatus": status(b.kind),
                "event": null,
            })
        })
        .collect();
    Ok(object(json!({ "list": list })))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    use tp_contracts::dav::{
        Collection, CollectionKind, Component, DavStore, ObjectIndex, Precondition, Span,
    };
    use tp_contracts::meta::{AccountScope, AccountStore, AccountUpsert};
    use tp_contracts::schedule::{BusyKind, BusyPeriod};

    use crate::testenv::Env;

    const USING: &[&str] = &[
        "urn:ietf:params:jmap:core",
        "urn:ietf:params:jmap:principals",
        "urn:ietf:params:jmap:principals:availability",
    ];

    async fn call(env: &Env, method: &str, args: Value) -> Value {
        let resp = env
            .request(json!({"using": USING, "methodCalls": [[method, args, "c0"]]}))
            .await;
        let name = &resp["methodResponses"][0][0];
        assert!(name == method || name == "error", "{resp}");
        resp["methodResponses"][0][1].clone()
    }

    fn at(s: &str) -> OffsetDateTime {
        OffsetDateTime::parse(s, &Rfc3339).expect("time")
    }

    /// Учётная запись организации с отображаемым именем.
    async fn user(env: &Env, login: &str, name: &str, enabled: bool) -> AccountScope {
        let scope = env.meta.account(login).await.expect("account");
        let cur = env
            .meta
            .get(scope.tenant, scope.account)
            .await
            .expect("get")
            .expect("account");
        let data = AccountUpsert {
            domain: cur.domain,
            login: cur.login,
            display_name: name.into(),
            external_id: cur.external_id,
            enabled,
            aliases: cur.aliases,
            kind: cur.kind,
            hidden_from_gal: false,
        };
        env.meta.upsert(scope.tenant, data).await.expect("upsert");
        scope
    }

    /// Событие с занятостью в календаре учётной записи (`transparent` — календарь вне занятости).
    async fn event(env: &Env, scope: AccountScope, uid: &str, busy: &[(&str, &str, BusyKind)]) {
        let cals = env
            .dav
            .list_collections(scope, CollectionKind::Calendar)
            .await
            .expect("list");
        let cal = match cals.first() {
            Some(c) => c.id,
            None => {
                let c = Collection::new(CollectionKind::Calendar, "default");
                let id = c.id;
                env.dav.create_collection(scope, c).await.expect("create");
                id
            }
        };
        let periods: Vec<BusyPeriod> = busy
            .iter()
            .map(|(s, e, kind)| BusyPeriod {
                start: at(s),
                end: at(e),
                kind: *kind,
            })
            .collect();
        let span = periods.first().map(|p| Span {
            start: p.start,
            end: periods.last().map(|l| l.end),
        });
        let mut index = ObjectIndex::new(Component::Event, uid, span);
        index.busy = periods;
        let body = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        env.dav
            .put_object(
                scope,
                cal,
                &format!("{uid}.ics"),
                body,
                index,
                Precondition::Any,
            )
            .await
            .expect("put");
    }

    #[tokio::test]
    async fn session_lists_capabilities() {
        let env = Env::new().await;
        let s = env
            .jmap
            .session(&env.principal, "https://mail.example.org")
            .await;
        let acc = env.account();
        for uri in USING.iter().skip(1) {
            assert!(s["capabilities"][uri].is_object(), "{uri}: {s}");
        }
        let caps = &s["accounts"][&acc]["accountCapabilities"];
        assert_eq!(
            caps[USING[1]],
            json!({"currentUserPrincipalId": acc}),
            "{s}"
        );
        assert_eq!(
            caps["urn:ietf:params:jmap:principals:owner"],
            json!({"accountIdForPrincipal": acc, "principalId": acc})
        );
        assert_eq!(
            caps[USING[2]],
            json!({"maxAvailabilityDuration": "PT34560000S"})
        );
        // Без хранилищ календарей и занятости capability нет.
        let bare = Env::bare().await;
        let s = bare
            .jmap
            .session(&bare.principal, "https://mail.example.org")
            .await;
        assert!(s["capabilities"][USING[1]].is_null(), "{s}");
        assert!(s["capabilities"][USING[2]].is_null(), "{s}");
    }

    #[tokio::test]
    async fn get_and_query() {
        let env = Env::new().await;
        let acc = env.account();
        user(&env, "user@example.org", "Иван Петров", true).await;
        let anna = user(&env, "anna@example.org", "Анна Смирнова", true).await;
        let room = user(&env, "room-3@example.org", "Переговорная 3", true).await;
        let off = user(&env, "gone@example.org", "Уволен", false).await;
        let id = |s: AccountScope| s.account.to_string();

        let g = call(&env, "Principal/get", json!({"accountId": acc})).await;
        let emails: Vec<&str> = g["list"]
            .as_array()
            .unwrap_or_else(|| panic!("{g}"))
            .iter()
            .filter_map(|p| p["email"].as_str())
            .collect();
        // Отключённая учётная запись — не участник.
        assert_eq!(
            emails,
            ["anna@example.org", "room-3@example.org", "user@example.org"]
        );
        let g = call(
            &env,
            "Principal/get",
            json!({"accountId": acc, "ids": [id(anna), acc, id(off), "nope"]}),
        )
        .await;
        assert_eq!(g["notFound"], json!([id(off), "nope"]), "{g}");
        assert_eq!(
            g["list"][0],
            json!({
                "id": id(anna), "type": "individual", "name": "Анна Смирнова", "description": null,
                "email": "anna@example.org", "timeZone": null, "accounts": null,
                "capabilities": {"urn:ietf:params:jmap:calendars": {
                    "accountId": null, "mayGetAvailability": true, "mayShareWith": false,
                    "calendarAddress": "mailto:anna@example.org"}},
            })
        );
        // Свой участник указывает на свою учётную запись.
        assert_eq!(
            g["list"][1]["capabilities"]["urn:ietf:params:jmap:calendars"]["accountId"],
            acc
        );

        let q = |args: Value| async {
            let mut a = args;
            a["accountId"] = json!(acc);
            call(&env, "Principal/query", a).await
        };
        let r = q(json!({})).await;
        assert_eq!(r["ids"], json!([id(anna), id(room), acc]), "{r}");
        assert_eq!(r["canCalculateChanges"], false);
        let r = q(
            json!({"sort": [{"property": "name", "isAscending": false}], "limit": 2,
                         "calculateTotal": true}),
        )
        .await;
        assert_eq!(r["ids"], json!([id(room), acc]), "{r}");
        assert_eq!(r["total"], 3);
        // Переговорная находится по имени и адресу (вид «ресурс» — после CTR-48).
        assert_eq!(
            q(json!({"filter": {"name": "переговорная"}})).await["ids"],
            json!([id(room)])
        );
        assert_eq!(
            q(json!({"filter": {"text": "ROOM-3"}})).await["ids"],
            json!([id(room)])
        );
        assert_eq!(
            q(json!({"filter": {"email": "anna@"}})).await["ids"],
            json!([id(anna)])
        );
        assert_eq!(
            q(json!({"filter": {"accountIds": [acc, id(anna)]}})).await["ids"],
            json!([acc])
        );
        assert_eq!(
            q(json!({"filter": {"type": "individual"}})).await["ids"]
                .as_array()
                .map(Vec::len),
            Some(3)
        );
        assert_eq!(
            q(json!({"filter": {"type": "resource"}})).await["ids"],
            json!([])
        );
        let both = json!({"operator": "OR", "conditions": [{"name": "анна"}, {"name": "иван"}]});
        assert_eq!(
            q(json!({"filter": both})).await["ids"],
            json!([id(anna), acc])
        );
        assert_eq!(
            q(json!({"filter": {"flavour": 1}})).await["type"],
            "unsupportedFilter"
        );
        assert_eq!(
            q(json!({"sort": [{"property": "type"}]})).await["type"],
            "unsupportedSort"
        );
    }

    /// Занятость двух пользователей: у каждого свои периоды, обрезанные по интервалу запроса.
    #[tokio::test]
    async fn availability_of_two_users() {
        let env = Env::new().await;
        let acc = env.account();
        let anna = user(&env, "anna@example.org", "Анна", true).await;
        let off = user(&env, "gone@example.org", "Уволен", false).await;
        event(
            &env,
            env.scope,
            "mine",
            &[(
                "2026-01-12T09:00:00Z",
                "2026-01-12T10:00:00Z",
                BusyKind::Busy,
            )],
        )
        .await;
        event(
            &env,
            anna,
            "a1",
            &[
                (
                    "2026-01-12T07:00:00Z",
                    "2026-01-12T08:30:00Z",
                    BusyKind::Busy,
                ),
                (
                    "2026-01-12T12:00:00Z",
                    "2026-01-12T13:00:00Z",
                    BusyKind::Tentative,
                ),
            ],
        )
        .await;
        event(
            &env,
            anna,
            "a2",
            &[(
                "2026-01-13T09:00:00Z",
                "2026-01-13T10:00:00Z",
                BusyKind::Busy,
            )],
        )
        .await;
        let day = |id: String| {
            json!({"accountId": acc, "id": id,
                   "utcStart": "2026-01-12T08:00:00Z", "utcEnd": "2026-01-13T00:00:00Z"})
        };

        let r = call(&env, "Principal/getAvailability", day(acc.clone())).await;
        assert_eq!(
            r["list"],
            json!([{"utcStart": "2026-01-12T09:00:00Z", "utcEnd": "2026-01-12T10:00:00Z",
                    "busyStatus": "confirmed", "event": null}]),
            "{r}"
        );
        let r = call(
            &env,
            "Principal/getAvailability",
            day(anna.account.to_string()),
        )
        .await;
        assert_eq!(
            r["list"],
            json!([
                {"utcStart": "2026-01-12T08:00:00Z", "utcEnd": "2026-01-12T08:30:00Z",
                 "busyStatus": "confirmed", "event": null},
                {"utcStart": "2026-01-12T12:00:00Z", "utcEnd": "2026-01-12T13:00:00Z",
                 "busyStatus": "tentative", "event": null},
            ]),
            "{r}"
        );

        for (patch, error) in [
            (json!({"id": off.account.to_string()}), "notFound"),
            (json!({"id": "nope"}), "notFound"),
            (
                json!({"utcEnd": "2026-01-12T08:00:00Z"}),
                "invalidArguments",
            ),
            (
                json!({"utcEnd": "2028-01-12T08:00:00Z"}),
                "invalidArguments",
            ),
            (
                json!({"utcStart": "2026-01-12T08:00:00"}),
                "invalidArguments",
            ),
            (json!({"showDetails": true}), "invalidArguments"),
        ] {
            let mut a = day(acc.clone());
            for (k, v) in patch.as_object().expect("patch") {
                a[k] = v.clone();
            }
            let r = call(&env, "Principal/getAvailability", a).await;
            assert_eq!(r["type"], error, "{r}");
        }
    }
}
