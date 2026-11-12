//! JMAP Calendars — календари (JMAP-27): `Calendar/get`, `/changes`, `/set` поверх [`DavStore`]
//! (вид [`CollectionKind::Calendar`]) — те же коллекции, что у CalDAV.
//!
//! Редакция: draft-ietf-jmap-calendars (текущий черновик IETF, SRS 4.4 «по мере стабилизации», решение 35) с
//! JSCalendar 2.0 (draft-ietf-calext-jscalendarbis — его реализует calcard). Отличия от черновика:
//! - `isSubscribed`, `isVisible` — читаются из коллекции (CTR-56); запись `false` — `invalidProperties` (JMAP-36);
//! - `isDefault` — назначенный календарь, иначе с сегментом URL `default`, иначе первый по порядку
//!   (`tp_contracts::dav::default_of`); `onSuccessSetIsDefault` для другого календаря — `invalidArguments` (JMAP-36);
//! - `includeInAvailability` — `all` или `none` (`schedule-calendar-transp` CalDAV, `Collection::transparent`);
//!   `attending` — `invalidProperties`;
//! - `defaultAlertsWithTime`, `defaultAlertsWithoutTime` — читаются из коллекции; запись — только `null` (JMAP-36);
//! - `shareWith`, `myRights` — права владельца, общий доступ — JMAP-32; `participantIdentities`,
//!   `CalendarEventNotification`, `Principal` — JMAP-30, JMAP-31.
//!
//! `name` — отображаемое имя коллекции, сегмент URL нового календаря — его `id`. Состояние —
//! [`DavStore::state`] вида (общее для `Calendar` и `CalendarEvent`); push — по `DavChanged`.

use std::str::FromStr;

use calcard::common::timezone::Tz;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tp_contracts::dav::{Collection, CollectionId, CollectionKind, DavStore, default_of};
use tp_contracts::meta::AccountScope;

use crate::contacts::store_error;
use crate::core::Ctx;
use crate::core::config::CalendarsConfig;
use crate::core::error::{MethodError, MethodResult, changes_error, set_error};
use crate::core::request::{
    ChangesArgs, GetArgs, SetArgs, SetOutcome, changes_response, map_changes, object, parse_args,
    parse_state, select, state_string,
};

pub(crate) const KIND: CollectionKind = CollectionKind::Calendar;

/// Сегмент URL календаря по умолчанию.
const DEFAULT_NAME: &str = tp_contracts::dav::DEFAULT_COLLECTION_NAME;

/// Значение capability учётной записи: событие — в одном календаре (объект CalDAV).
pub(crate) fn account_capability(cfg: &CalendarsConfig) -> Value {
    json!({
        "maxCalendarsPerEvent": 1,
        "minDateTime": "0001-01-01T00:00:00",
        "maxDateTime": "9999-12-31T23:59:59",
        "maxExpandedQueryDuration": format!("PT{}S", cfg.max_expanded_query_duration.as_secs()),
        "maxParticipantsPerEvent": null,
        "mayCreateCalendar": true,
    })
}

/// Календарь по умолчанию среди `books` — правило контракта [`default_of`]: назначенный, иначе прежнее правило.
pub(crate) fn default_calendar(books: &[Collection]) -> Option<CollectionId> {
    default_of(books).map(|b| b.id)
}

/// Оповещения по умолчанию календаря (текст JSON-объекта `Id[Alert]`) — значением JMAP; нет — `null`.
fn alerts(text: Option<&str>) -> Value {
    text.and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or(Value::Null)
}

/// Текущее состояние календарей.
pub(crate) async fn state(store: &dyn DavStore, scope: AccountScope) -> MethodResult<String> {
    Ok(state_string(store.state(scope, KIND).await?))
}

/// `includeInAvailability`: календарь вне занятости (`schedule-calendar-transp: transparent`) — `none`.
fn availability(c: &Collection) -> &'static str {
    if c.transparent { "none" } else { "all" }
}

fn rights() -> Value {
    json!({
        "mayReadFreeBusy": true, "mayReadItems": true, "mayWriteAll": true, "mayWriteOwn": true,
        "mayUpdatePrivate": true, "mayRSVP": true, "mayShare": false, "mayDelete": true,
    })
}

fn calendar_object(b: &Collection, default: Option<CollectionId>) -> Map<String, Value> {
    object(json!({
        "id": b.id.to_string(),
        "name": b.display_name,
        "description": b.description,
        "sortOrder": b.order,
        "color": b.color,
        "isDefault": default == Some(b.id),
        "isSubscribed": b.subscribed,
        "isVisible": b.visible,
        "includeInAvailability": availability(b),
        "defaultAlertsWithTime": alerts(b.alerts_with_time.as_deref()),
        "defaultAlertsWithoutTime": alerts(b.alerts_without_time.as_deref()),
        "timeZone": b.timezone,
        "shareWith": null,
        "myRights": rights(),
    }))
}

/// `Calendar/get`.
pub(crate) async fn get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let store = ctx.jmap.dav_store()?;
    let st = state(store.as_ref(), ctx.scope).await?;
    let books = store.list_collections(ctx.scope, KIND).await?;
    let default = default_calendar(&books);
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    let props = a.properties.as_deref();
    match &a.ids {
        None => {
            list.extend(
                books
                    .iter()
                    .map(|b| select(calendar_object(b, default), props)),
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
                match books.iter().find(|b| b.id.to_string() == id) {
                    Some(b) => list.push(select(calendar_object(b, default), props)),
                    None => not_found.push(id.to_owned()),
                }
            }
        }
    }
    Ok(object(json!({
        "accountId": a.account_id,
        "state": st,
        "list": list,
        "notFound": not_found,
    })))
}

/// `Calendar/changes` по журналу коллекций (изменения событий — `CalendarEvent/changes`).
pub(crate) async fn changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: ChangesArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let limit = a.limit(ctx.jmap.cfg.max_changes)?;
    let since = parse_state(&a.since_state)?;
    let c = ctx
        .jmap
        .dav_store()?
        .collection_changes(ctx.scope, KIND, since, limit)
        .await
        .map_err(changes_error)?;
    let c = map_changes(c, ToString::to_string);
    let mut out = changes_response(&a.account_id, &a.since_state, &c);
    out.insert("updatedProperties".into(), Value::Null);
    Ok(out)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalendarSetArgs {
    #[serde(default)]
    on_destroy_remove_events: bool,
    on_success_set_is_default: Option<String>,
}

/// Применяет свойства `/set` к коллекции; ошибка — `invalidProperties`.
fn apply_props(c: &mut Collection, v: &Value, create: bool) -> Result<(), Value> {
    let Value::Object(p) = v else {
        return Err(set_error("invalidProperties", None, &[]));
    };
    let mut bad: Vec<&str> = Vec::new();
    let mut named = !create;
    for (key, val) in p {
        let ok = match (key.as_str(), val) {
            ("name", Value::String(s)) if !s.trim().is_empty() => {
                c.display_name = s.clone();
                named = true;
                true
            }
            ("description", Value::Null) => {
                c.description = None;
                true
            }
            ("description", Value::String(s)) => {
                c.description = Some(s.clone());
                true
            }
            ("sortOrder", Value::Number(n)) => n
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .map(|n| c.order = n)
                .is_some(),
            ("color", Value::Null) => {
                c.color = None;
                true
            }
            ("color", Value::String(s)) => {
                c.color = Some(s.clone());
                true
            }
            ("timeZone", Value::Null) => {
                c.timezone = None;
                true
            }
            ("timeZone", Value::String(s)) if Tz::from_str(s).is_ok() => {
                c.timezone = Some(s.clone());
                true
            }
            ("includeInAvailability", Value::String(s)) if s == "all" || s == "none" => {
                c.transparent = s == "none";
                true
            }
            ("isSubscribed" | "isVisible", Value::Bool(true))
            | ("shareWith" | "defaultAlertsWithTime" | "defaultAlertsWithoutTime", Value::Null) => {
                true
            }
            _ => false,
        };
        if !ok {
            bad.push(key);
        }
    }
    if !named {
        bad.push("name");
    }
    if !bad.is_empty() {
        return Err(set_error("invalidProperties", None, &bad));
    }
    c.validate().map_err(|e| {
        let why = e.to_string();
        set_error("invalidProperties", Some(&why), &[])
    })
}

/// `Calendar/set`: создание, изменение (имя, описание, цвет, порядок, пояс, `includeInAvailability`), удаление
/// (`onDestroyRemoveEvents`).
pub(crate) async fn set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let mut rest = args.clone();
    rest.retain(|k, _| k.starts_with("on"));
    let extra: CalendarSetArgs = parse_args(rest)?;
    let mut base = args;
    base.retain(|k, _| !k.starts_with("on"));
    let a: SetArgs = parse_args(base)?;
    ctx.check_account(&a.account_id)?;
    a.check_limit(ctx.jmap.cfg.max_objects_in_set)?;
    let store = ctx.jmap.dav_store()?.clone();
    let old = state(store.as_ref(), ctx.scope).await?;
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }
    let before = store.list_collections(ctx.scope, KIND).await?;
    if let Some(want) = &extra.on_success_set_is_default {
        // Признак не хранится: допустимо только то, что и так станет календарём по умолчанию.
        let ok = match want.strip_prefix('#') {
            Some(cid) => {
                before.is_empty() && a.create.as_ref().is_some_and(|c| c.contains_key(cid))
            }
            None => default_calendar(&before).is_some_and(|d| d.to_string() == *want),
        };
        if !ok {
            return Err(MethodError::invalid(
                "onSuccessSetIsDefault: changing the default calendar is not supported",
            ));
        }
    }
    let mut out = SetOutcome::default();
    for (cid, v) in a.create.unwrap_or_default() {
        let mut c = Collection::new(KIND, DEFAULT_NAME);
        c.name = c.id.to_string();
        if let Err(e) = apply_props(&mut c, &v, true) {
            out.not_created.insert(cid, e);
            continue;
        }
        let (id, include) = (c.id, availability(&c));
        match store.create_collection(ctx.scope, c).await {
            Ok(_) => {
                ctx.created.insert(cid.clone(), id.to_string());
                let books = store.list_collections(ctx.scope, KIND).await?;
                out.created.insert(
                    cid,
                    json!({
                        "id": id.to_string(),
                        "isDefault": default_calendar(&books) == Some(id),
                        "isSubscribed": true,
                        "isVisible": true,
                        "includeInAvailability": include,
                        "defaultAlertsWithTime": null,
                        "defaultAlertsWithoutTime": null,
                        "shareWith": null,
                        "myRights": rights(),
                    }),
                );
            }
            Err(e) => {
                out.not_created.insert(cid, store_error(e)?);
            }
        }
    }
    for (id, patch) in a.update.unwrap_or_default() {
        let rid = ctx.resolve_id(&id).to_owned();
        let cur = match CollectionId::from_str(&rid) {
            Ok(cid) => store
                .get_collection(ctx.scope, cid)
                .await?
                .filter(|c| c.kind == KIND),
            Err(_) => None,
        };
        let Some(mut c) = cur else {
            out.not_updated.insert(id, set_error("notFound", None, &[]));
            continue;
        };
        if let Err(e) = apply_props(&mut c, &patch, false) {
            out.not_updated.insert(id, e);
            continue;
        }
        match store.update_collection(ctx.scope, c).await {
            Ok(_) => out.updated.insert(id, Value::Null),
            Err(e) => out.not_updated.insert(id, store_error(e)?),
        };
    }
    for id in a.destroy.unwrap_or_default() {
        let rid = ctx.resolve_id(&id).to_owned();
        let cur = match CollectionId::from_str(&rid) {
            Ok(cid) => store
                .get_collection(ctx.scope, cid)
                .await?
                .filter(|c| c.kind == KIND),
            Err(_) => None,
        };
        let Some(c) = cur else {
            out.not_destroyed
                .insert(id, set_error("notFound", None, &[]));
            continue;
        };
        if !extra.on_destroy_remove_events && !store.list_objects(ctx.scope, c.id).await?.is_empty()
        {
            out.not_destroyed
                .insert(id, set_error("calendarHasEvent", None, &[]));
            continue;
        }
        match store.delete_collection(ctx.scope, c.id).await {
            Ok(_) => out.destroyed.push(id),
            Err(e) => {
                out.not_destroyed.insert(id, store_error(e)?);
            }
        }
    }
    let new = state(store.as_ref(), ctx.scope).await?;
    Ok(out.response(&a.account_id, &old, &new))
}

#[cfg(test)]
pub(crate) mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::{Value, json};
    use tp_contracts::dav::{CollectionKind, DavStore};

    use crate::testenv::{Env, http10};

    pub(crate) const USING: &[&str] = &[
        "urn:ietf:params:jmap:core",
        "urn:ietf:params:jmap:calendars",
    ];

    pub(crate) async fn call(env: &Env, method: &str, args: Value) -> Value {
        let resp = env
            .request(json!({"using": USING, "methodCalls": [[method, args, "c0"]]}))
            .await;
        let name = &resp["methodResponses"][0][0];
        assert!(name == method || name == "error", "{resp}");
        resp["methodResponses"][0][1].clone()
    }

    pub(crate) async fn create_calendar(env: &Env, name: &str) -> String {
        let r = call(
            env,
            "Calendar/set",
            json!({"accountId": env.account(), "create": {"c": {"name": name}}}),
        )
        .await;
        r["created"]["c"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{r}"))
            .to_owned()
    }

    /// Запрос CalDAV к серверу `tp_dav` поверх того же хранилища.
    pub(crate) async fn dav_request(
        addr: std::net::SocketAddr,
        method: &str,
        path: &str,
        body: &str,
    ) -> (u16, String) {
        let auth = STANDARD.encode("user@example.org:app-secret");
        let req = format!(
            "{method} {path} HTTP/1.0\r\nHost: dav.test\r\nAuthorization: Basic {auth}\r\n\
             Content-Type: text/calendar; charset=utf-8\r\nDepth: 1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let (status, _, body) = http10(addr, req.as_bytes()).await;
        (status, String::from_utf8(body).expect("utf-8"))
    }

    /// Путь календаря в CalDAV.
    pub(crate) async fn calendar_path(env: &Env, calendar: &str) -> String {
        let c = env
            .dav
            .get_collection(env.scope, calendar.parse().expect("id"))
            .await
            .expect("get")
            .expect("calendar");
        format!("/dav/calendars/user@example.org/{}/", c.name)
    }

    #[tokio::test]
    async fn session_lists_capability() {
        let env = Env::new().await;
        let s = env.jmap.session(&env.principal, "https://mail.example.org");
        let acc = env.account();
        assert!(s["capabilities"][USING[1]].is_object(), "{s}");
        let cap = &s["accounts"][&acc]["accountCapabilities"][USING[1]];
        assert_eq!(cap["maxCalendarsPerEvent"], 1, "{s}");
        assert_eq!(cap["mayCreateCalendar"], true);
        assert_eq!(cap["maxExpandedQueryDuration"], "PT34560000S");
        assert_eq!(s["primaryAccounts"][USING[1]], acc);
        // Без хранилища DAV capability нет, метод неизвестен.
        let bare = Env::bare().await;
        let s = bare
            .jmap
            .session(&bare.principal, "https://mail.example.org");
        assert!(s["capabilities"][USING[1]].is_null(), "{s}");
    }

    #[tokio::test]
    async fn create_update_changes() {
        let env = Env::new().await;
        let acc = env.account();
        let r = call(&env, "Calendar/get", json!({"accountId": acc})).await;
        assert_eq!(r["list"], json!([]), "{r}");
        let state0 = r["state"].as_str().expect("state").to_owned();

        let r = call(
            &env,
            "Calendar/set",
            json!({"accountId": acc, "ifInState": state0, "create": {
                "c": {"name": "Работа", "color": "#ff0000", "sortOrder": 2, "timeZone": "Europe/Moscow",
                      "isSubscribed": true, "includeInAvailability": "none"},
                "bad": {"color": 5, "includeInAvailability": "attending", "isSubscribed": false,
                        "timeZone": "Mars/Olympus"}}}),
        )
        .await;
        let id = r["created"]["c"]["id"].as_str().expect("id").to_owned();
        assert_eq!(r["created"]["c"]["isDefault"], true, "{r}");
        assert_eq!(r["created"]["c"]["includeInAvailability"], "none", "{r}");
        assert_eq!(r["notCreated"]["bad"]["type"], "invalidProperties", "{r}");
        assert_eq!(
            r["notCreated"]["bad"]["properties"],
            json!([
                "color",
                "includeInAvailability",
                "isSubscribed",
                "timeZone",
                "name"
            ])
        );
        let state1 = r["newState"].as_str().expect("state").to_owned();
        assert_ne!(state0, state1);

        // Та же коллекция видна CalDAV (общее хранилище): цвет, порядок, пояс, занятость.
        let cals = env
            .dav
            .list_collections(env.scope, CollectionKind::Calendar)
            .await
            .expect("list");
        assert_eq!(cals[0].display_name, "Работа");
        assert_eq!(cals[0].name, id);
        assert_eq!(cals[0].color.as_deref(), Some("#ff0000"));
        assert_eq!(cals[0].timezone.as_deref(), Some("Europe/Moscow"));
        assert!(cals[0].transparent);

        let r = call(
            &env,
            "Calendar/set",
            json!({"accountId": acc, "update": {
                &id: {"name": "Личное", "color": null, "sortOrder": 7, "includeInAvailability": "all"},
                "nope": {"name": "x"}}}),
        )
        .await;
        assert!(
            r["updated"]
                .as_object()
                .is_some_and(|u| u.contains_key(&id)),
            "{r}"
        );
        assert_eq!(r["notUpdated"]["nope"]["type"], "notFound", "{r}");
        let g = call(
            &env,
            "Calendar/get",
            json!({"accountId": acc, "ids": [id, "zz"]}),
        )
        .await;
        assert_eq!(g["notFound"], json!(["zz"]));
        assert_eq!(
            g["list"][0],
            json!({
                "id": id, "name": "Личное", "description": null, "color": null, "sortOrder": 7,
                "isDefault": true, "isSubscribed": true, "isVisible": true,
                "includeInAvailability": "all", "defaultAlertsWithTime": null,
                "defaultAlertsWithoutTime": null, "timeZone": "Europe/Moscow", "shareWith": null,
                "myRights": {"mayReadFreeBusy": true, "mayReadItems": true, "mayWriteAll": true,
                    "mayWriteOwn": true, "mayUpdatePrivate": true, "mayRSVP": true, "mayShare": false,
                    "mayDelete": true},
            })
        );

        let c = call(
            &env,
            "Calendar/changes",
            json!({"accountId": acc, "sinceState": state0}),
        )
        .await;
        assert_eq!(c["created"], json!([id]), "{c}");
        let c = call(
            &env,
            "Calendar/changes",
            json!({"accountId": acc, "sinceState": state1}),
        )
        .await;
        assert_eq!(c["updated"], json!([id]), "{c}");
        assert_eq!(c["created"], json!([]));
        assert_eq!(c["hasMoreChanges"], false);
        assert_eq!(c["newState"], g["state"]);
        let c = call(
            &env,
            "Calendar/changes",
            json!({"accountId": acc, "sinceState": "nope"}),
        )
        .await;
        assert_eq!(c["type"], "cannotCalculateChanges", "{c}");
    }

    /// Календарь, созданный CalDAV (`MKCALENDAR`), виден в JMAP; адресные книги в `Calendar/get` не попадают.
    #[tokio::test]
    async fn caldav_calendar_is_visible() {
        let env = Env::new().await;
        let acc = env.account();
        let addr = crate::contact_card::tests::dav(&env).await;
        let body = r##"<?xml version="1.0" encoding="utf-8"?>
<C:mkcalendar xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:A="http://apple.com/ns/ical/">
  <D:set><D:prop>
    <D:displayname>Команда</D:displayname>
    <C:calendar-description>Общие встречи</C:calendar-description>
    <A:calendar-color>#00ff00</A:calendar-color>
  </D:prop></D:set>
</C:mkcalendar>"##;
        let (status, text) = dav_request(
            addr,
            "MKCALENDAR",
            "/dav/calendars/user@example.org/team/",
            body,
        )
        .await;
        assert_eq!(status, 201, "{text}");
        crate::contacts::tests::create_book(&env, "Книга").await;

        let g = call(&env, "Calendar/get", json!({"accountId": acc})).await;
        let list = g["list"].as_array().expect("list");
        assert_eq!(list.len(), 1, "{g}");
        assert_eq!(list[0]["name"], "Команда", "{g}");
        assert_eq!(list[0]["description"], "Общие встречи");
        assert_eq!(list[0]["color"], "#00ff00");
        let id = list[0]["id"].as_str().expect("id").to_owned();
        assert_eq!(
            calendar_path(&env, &id).await,
            "/dav/calendars/user@example.org/team/"
        );

        // Переименование из JMAP видно CalDAV.
        let r = call(
            &env,
            "Calendar/set",
            json!({"accountId": acc, "update": {&id: {"name": "Отдел"}}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        let propfind = r#"<?xml version="1.0"?><D:propfind xmlns:D="DAV:"><D:prop><D:displayname/></D:prop></D:propfind>"#;
        let (status, text) = dav_request(
            addr,
            "PROPFIND",
            "/dav/calendars/user@example.org/team/",
            propfind,
        )
        .await;
        assert_eq!(status, 207, "{text}");
        assert!(text.contains("Отдел"), "{text}");
    }

    #[tokio::test]
    async fn destroy_with_and_without_events() {
        let env = Env::new().await;
        let acc = env.account();
        let addr = crate::contact_card::tests::dav(&env).await;
        let empty = create_calendar(&env, "Пустой").await;
        let full = create_calendar(&env, "С событиями").await;
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//RU\r\nBEGIN:VEVENT\r\nUID:e1\r\n\
                   DTSTAMP:20260101T000000Z\r\nDTSTART:20260105T100000Z\r\nSUMMARY:X\r\nEND:VEVENT\r\n\
                   END:VCALENDAR\r\n";
        let path = format!("{}e1.ics", calendar_path(&env, &full).await);
        let (status, text) = dav_request(addr, "PUT", &path, ics).await;
        assert_eq!(status, 201, "{text}");

        let r = call(
            &env,
            "Calendar/set",
            json!({"accountId": acc, "destroy": [empty, full, "nope"]}),
        )
        .await;
        assert_eq!(r["destroyed"], json!([empty]), "{r}");
        assert_eq!(r["notDestroyed"][&full]["type"], "calendarHasEvent", "{r}");
        assert_eq!(r["notDestroyed"]["nope"]["type"], "notFound", "{r}");
        let r = call(
            &env,
            "Calendar/set",
            json!({"accountId": acc, "destroy": [full], "onDestroyRemoveEvents": true}),
        )
        .await;
        assert_eq!(r["destroyed"], json!([full]), "{r}");
    }

    #[tokio::test]
    async fn default_calendar() {
        let env = Env::new().await;
        let acc = env.account();
        let a = create_calendar(&env, "A").await;
        let b = create_calendar(&env, "B").await;
        let g = call(&env, "Calendar/get", json!({"accountId": acc})).await;
        let defaults: Vec<&Value> = g["list"]
            .as_array()
            .expect("list")
            .iter()
            .filter(|x| x["isDefault"] == true)
            .collect();
        assert_eq!(defaults.len(), 1, "{g}");
        let default = defaults[0]["id"].as_str().expect("id");
        let other = if default == a { &b } else { &a };
        let r = call(
            &env,
            "Calendar/set",
            json!({"accountId": acc, "onSuccessSetIsDefault": default}),
        )
        .await;
        assert!(r["newState"].is_string(), "{r}");
        let r = call(
            &env,
            "Calendar/set",
            json!({"accountId": acc, "onSuccessSetIsDefault": other}),
        )
        .await;
        assert_eq!(r["type"], "invalidArguments", "{r}");
    }

    /// Изменение календаря → push `Calendar`, `CalendarEvent`; состояние контактов не меняется.
    #[tokio::test]
    async fn push_types() {
        use futures::StreamExt;
        use tp_contracts::bus::EventBus;
        let env = Env::new().await;
        let contacts = env
            .dav
            .state(env.scope, CollectionKind::AddressBook)
            .await
            .expect("state");
        let mut events = env.bus.subscribe(env.scope).await.expect("subscribe");
        create_calendar(&env, "A").await;
        let ev = events.next().await.expect("event");
        assert_eq!(
            crate::push::changed_types(&ev.kind),
            ["Calendar", "CalendarEvent"],
            "{ev:?}"
        );
        assert_eq!(
            env.dav
                .state(env.scope, CollectionKind::AddressBook)
                .await
                .expect("state"),
            contacts
        );
    }
}
