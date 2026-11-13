//! JMAP Calendars — планирование встреч (JMAP-31): рассылка iTIP после `CalendarEvent/set` с
//! `sendSchedulingMessages`, `ParticipantIdentity/get`, `CalendarEventNotification/get`, `/changes`, `/set`.
//!
//! Своего iTIP здесь нет: сообщения строит `tp_dav::itip::plan` (организатор — `REQUEST` и `CANCEL`, участник —
//! `REPLY` при изменении своего `participationStatus` или удалении события), рассылает
//! `tp_dav::ScheduleDeps::dispatch` (учётным записям организации — `ItipDelivery`, внешним — `ItipSender`); итог —
//! `SCHEDULE-STATUS` в объекте, как после `PUT` CalDAV. Планирование подключает [`Jmap::with_schedule`]; без него
//! `sendSchedulingMessages: true` — `invalidArguments`.
//!
//! `CalendarEventNotification` — сообщения входящих планирования (`DavStore::inbox_list`): `id` — идентификатор
//! ресурса входящих, `state` — `DavStore::inbox_state`, `/changes` — по журналу входящих `DavStore::inbox_changes`
//! (CTR-65; JMAP-42). Отличия от черновика:
//! - уведомление — входящее сообщение iTIP, а не запись о чужой правке общего календаря (общие календари — JMAP-32);
//!   применённые сервером `REQUEST`, `REPLY`, `CANCEL` во входящих остаются только при `dav.schedule_keep_processed`;
//! - `type`: `CANCEL` — `destroyed`; `REQUEST`, `ADD`, `PUBLISH` с `sequence` 0 — `created`; остальное — `updated`;
//! - `event` — событие из сообщения (у `updated` — новое состояние, `eventPatch` — `null`: прежнего нет);
//! - `created` — время поступления во входящие (`InboxItem::received_at`);
//! - `/query`: фильтр `after`, `before` (по `created`), `type`, `calendarEventIds` и операторы, сортировка — по
//!   `created` (по умолчанию — по возрастанию); `canCalculateChanges` — `false`, `/queryChanges` не поддерживается;
//! - `ParticipantIdentity` — один адрес (имя входа); `/changes`, `/set` не поддерживаются.

use std::str::FromStr;

use calcard::icalendar::ICalendar;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tp_contracts::dav::{
    CollectionId, DavObjectId, DavStore, ObjectIndex, Precondition, PutOutcome,
};
use tp_contracts::schedule::{InboxItem, ItipMethod};
use tp_dav::itip;

use crate::calendar_event::{calendar_ids, split};
use crate::core::error::{MethodError, MethodResult, changes_error, set_error};
use crate::core::request::{
    ChangesArgs, Comparator, GetArgs, QueryWindow, SetArgs, SetOutcome, apply_window,
    changes_response, map_changes, object, parse_args, parse_state, select, state_string,
};
use crate::core::{Ctx, Jmap};
use crate::email_get::utc_date;
use crate::email_query::utc;

/// Адреса календарного пользователя вошедшего (`tp_dav::itip::address`): имя входа, если оно — адрес (как в CalDAV).
pub(crate) fn addresses(ctx: &Ctx<'_>) -> Vec<String> {
    let login = itip::address(&ctx.principal.login);
    if login.contains('@') {
        vec![login]
    } else {
        Vec::new()
    }
}

/// `isOrigin`: событие не объект планирования или вошедший — его организатор.
pub(crate) fn is_origin(body: &str, me: &[String]) -> bool {
    let Some(m) = ICalendar::parse(body).ok().as_ref().and_then(itip::meeting) else {
        return true;
    };
    !m.is_scheduling() || m.organizer.as_ref().is_none_or(|o| me.contains(o))
}

/// Записанный объект события: куда дописать `SCHEDULE-STATUS`.
pub(crate) struct Written<'a> {
    pub collection: CollectionId,
    pub name: &'a str,
    pub body: &'a str,
    pub index: ObjectIndex,
    pub etag: String,
}

/// Рассылка iTIP после изменения события вошедшим: `old` — прежнее тело (`None` — создание), `new` — записанный
/// объект (`None` — удаление). Итоги рассылки дописываются в объект; сбой рассылки запись события не отменяет
/// (статус `5.x` у участника). Планирование не подключено — ничего не делает.
pub(crate) async fn announce(ctx: &Ctx<'_>, old: Option<&str>, new: Option<Written<'_>>) {
    let (Some(deps), Some(store)) = (&ctx.jmap.schedule, &ctx.jmap.dav) else {
        return;
    };
    let parse = |b: &str| ICalendar::parse(b).ok();
    let old = old.and_then(parse);
    let new_ical = new.as_ref().and_then(|w| parse(w.body));
    let plan = itip::plan(
        old.as_ref(),
        new_ical.as_ref(),
        &addresses(ctx),
        OffsetDateTime::now_utc(),
    );
    let from = match plan.role {
        itip::Role::Organizer(a) | itip::Role::Attendee(a) => a,
        itip::Role::None => return,
    };
    if plan.messages.is_empty() {
        return;
    }
    let statuses = deps.dispatch(ctx.scope, &from, &plan.messages).await;
    let (Some(w), Some(mut ical)) = (new, new_ical) else {
        return;
    };
    itip::set_schedule_status(&mut ical, &statuses);
    // Одновременная правка клиентом важнее итогов рассылки: её не перезаписываем.
    let pre = Precondition::IfMatch(w.etag);
    match store
        .put_object(
            ctx.scope,
            w.collection,
            w.name,
            ical.to_string(),
            w.index,
            pre,
        )
        .await
    {
        Ok(PutOutcome::Stored(_)) => {}
        Ok(_) => tracing::debug!("JMAP: SCHEDULE-STATUS не записан — событие изменено"),
        Err(e) => tracing::warn!(error = %e, "JMAP: SCHEDULE-STATUS не записан"),
    }
}

// ---------- ParticipantIdentity ----------

/// Состояние `ParticipantIdentity`: адреса задаёт учётная запись, из JMAP они не меняются.
const IDENTITY_STATE: &str = "0";

/// `ParticipantIdentity/get`: адреса учётной записи, от имени которых она участвует во встречах.
pub(crate) async fn identity_get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    ctx.jmap.dav_store()?;
    let p = ctx.principal;
    let name = ctx
        .jmap
        .d
        .accounts
        .get(p.tenant, p.account)
        .await?
        .map(|acc| acc.display_name)
        .unwrap_or_default();
    let all: Vec<Map<String, Value>> = addresses(ctx)
        .into_iter()
        .map(|addr| {
            object(json!({
                "id": p.account.to_string(),
                "name": name,
                "calendarAddress": format!("mailto:{addr}"),
                "isDefault": true,
            }))
        })
        .collect();
    let props = a.properties.as_deref();
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    match &a.ids {
        None => list.extend(all.into_iter().map(|m| select(m, props))),
        Some(ids) => {
            let mut seen: Vec<&str> = Vec::new();
            for id in ids {
                let rid = ctx.resolve_id(id);
                if seen.contains(&rid) {
                    continue;
                }
                seen.push(rid);
                match all.iter().find(|m| m["id"] == rid) {
                    Some(m) => list.push(select(m.clone(), props)),
                    None => not_found.push(rid.to_owned()),
                }
            }
        }
    }
    Ok(object(json!({
        "accountId": a.account_id,
        "state": IDENTITY_STATE,
        "list": list,
        "notFound": not_found,
    })))
}

// ---------- CalendarEventNotification ----------

/// Имя и адрес участника события с адресом `addr`.
fn participant_name(event: &Map<String, Value>, addr: &str) -> Option<String> {
    event
        .get("participants")?
        .as_object()?
        .values()
        .find(|p| {
            p.get("calendarAddress")
                .and_then(Value::as_str)
                .is_some_and(|a| itip::address(a) == addr)
        })?
        .get("name")?
        .as_str()
        .map(str::to_owned)
}

/// `id` события календарей `cals` учётной записи с UID `uid`.
async fn event_id(
    ctx: &Ctx<'_>,
    store: &dyn DavStore,
    cals: &[CollectionId],
    uid: &str,
) -> MethodResult<Option<String>> {
    for &cal in cals {
        if let Some(o) = store.object_by_uid(ctx.scope, cal, uid).await? {
            return Ok(Some(o.id.to_string()));
        }
    }
    Ok(None)
}

/// Событие JSCalendar из сообщения входящих.
fn message_event(item: &InboxItem) -> Map<String, Value> {
    let msg = &item.message;
    let mut event = split(&msg.body).map(|(_, e)| e).unwrap_or_default();
    event.entry("@type").or_insert_with(|| json!("Event"));
    event.entry("uid").or_insert_with(|| json!(msg.uid));
    event
}

/// `type` уведомления по методу сообщения и `sequence` события.
fn kind(item: &InboxItem, event: &Map<String, Value>) -> &'static str {
    let first = event.get("sequence").and_then(Value::as_u64).unwrap_or(0) == 0;
    match item.message.method {
        ItipMethod::Cancel => "destroyed",
        ItipMethod::Request | ItipMethod::Add | ItipMethod::Publish if first => "created",
        _ => "updated",
    }
}

/// Уведомление из сообщения входящих планирования; `cals` — календари учётной записи.
async fn notification(
    ctx: &Ctx<'_>,
    store: &dyn DavStore,
    cals: &[CollectionId],
    item: &InboxItem,
) -> MethodResult<Map<String, Value>> {
    let msg = &item.message;
    let event = message_event(item);
    let kind = kind(item, &event);
    let from = itip::address(&msg.from);
    let principal = ctx
        .jmap
        .d
        .accounts
        .by_login(ctx.scope.tenant, &from)
        .await?
        .map(|a| a.id.to_string());
    Ok(object(json!({
        "id": item.id.to_string(),
        "created": utc_date(item.received_at),
        "changedBy": {
            "name": participant_name(&event, &from).unwrap_or_default(),
            "email": from,
            "principalId": principal,
            "calendarAddress": format!("mailto:{from}"),
        },
        "comment": null,
        "type": kind,
        "calendarEventId": event_id(ctx, store, cals, &msg.uid).await?,
        "isDraft": false,
        "event": event,
        "eventPatch": null,
    })))
}

/// `CalendarEventNotification/get`.
pub(crate) async fn notification_get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let store = ctx.jmap.dav_store()?.clone();
    let inbox = store.inbox_list(ctx.scope).await?;
    let props = a.properties.as_deref();
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    let picked: Vec<&InboxItem> = match &a.ids {
        None => inbox.items.iter().collect(),
        Some(ids) => {
            let mut out = Vec::new();
            let mut seen: Vec<&str> = Vec::new();
            for id in ids {
                let rid = ctx.resolve_id(id);
                if seen.contains(&rid) {
                    continue;
                }
                seen.push(rid);
                let oid = DavObjectId::from_str(rid).ok();
                match inbox.items.iter().find(|i| Some(i.id) == oid) {
                    Some(i) => out.push(i),
                    None => not_found.push(rid.to_owned()),
                }
            }
            out
        }
    };
    let cals = calendar_ids(store.as_ref(), ctx.scope).await?;
    for item in picked {
        list.push(select(
            notification(ctx, store.as_ref(), &cals, item).await?,
            props,
        ));
    }
    Ok(object(json!({
        "accountId": a.account_id,
        "state": state_string(inbox.state),
        "list": list,
        "notFound": not_found,
    })))
}

/// `CalendarEventNotification/changes` по журналу входящих планирования (`DavStore::inbox_changes`).
pub(crate) async fn notification_changes(
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
        .inbox_changes(ctx.scope, since, limit)
        .await
        .map_err(changes_error)?;
    let c = map_changes(c, ToString::to_string);
    Ok(changes_response(&a.account_id, &a.since_state, &c))
}

/// Свойства уведомления, по которым фильтрует и сортирует `/query`.
struct Facts {
    id: String,
    created: OffsetDateTime,
    kind: &'static str,
    event: Option<String>,
}

/// Условие `key` есть в фильтре (в том числе внутри операторов).
fn uses(filter: &Value, key: &str) -> bool {
    filter.get(key).is_some()
        || filter
            .get("conditions")
            .and_then(Value::as_array)
            .is_some_and(|c| c.iter().any(|f| uses(f, key)))
}

/// Проверка фильтра `/query` (`FilterOperator` или `FilterCondition`).
fn matches(filter: &Value, n: &Facts) -> MethodResult<bool> {
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
            results.push(matches(c, n)?);
        }
        return match op.as_str() {
            Some("AND") => Ok(results.iter().all(|r| *r)),
            Some("OR") => Ok(results.iter().any(|r| *r)),
            Some("NOT") => Ok(!results.iter().any(|r| *r)),
            _ => Err(MethodError::invalid("unknown filter operator")),
        };
    }
    let mut ok = true;
    for (key, val) in f {
        let unsupported = || MethodError::with("unsupportedFilter", key.clone());
        let date = || val.as_str().ok_or_else(unsupported).and_then(utc);
        ok &= match key.as_str() {
            "after" => n.created >= date()?,
            "before" => n.created < date()?,
            "type" => val.as_str().ok_or_else(unsupported)? == n.kind,
            "calendarEventIds" => {
                let ids = val.as_array().ok_or_else(unsupported)?;
                n.event
                    .as_deref()
                    .is_some_and(|e| ids.iter().any(|id| id.as_str() == Some(e)))
            }
            _ => return Err(unsupported()),
        };
    }
    Ok(ok)
}

/// Аргументы `CalendarEventNotification/query`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryArgs {
    account_id: String,
    filter: Option<Value>,
    sort: Option<Vec<Comparator>>,
    #[serde(flatten)]
    window: QueryWindow,
}

/// `CalendarEventNotification/query` по входящим планирования (без `/queryChanges`).
pub(crate) async fn notification_query(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: QueryArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let sort = a.sort.as_deref().unwrap_or_default();
    if sort.iter().any(|c| c.property != "created") {
        return Err(MethodError::new("unsupportedSort"));
    }
    let descending = sort.first().is_some_and(|c| !c.is_ascending);
    let filter = a.filter.as_ref().filter(|f| !f.is_null());
    let store = ctx.jmap.dav_store()?.clone();
    let inbox = store.inbox_list(ctx.scope).await?;
    // `calendarEventId` ищется по календарям, только если фильтр по нему.
    let cals = match filter {
        Some(f) if uses(f, "calendarEventIds") => calendar_ids(store.as_ref(), ctx.scope).await?,
        _ => Vec::new(),
    };
    let mut found = Vec::new();
    for item in &inbox.items {
        let event = if cals.is_empty() {
            None
        } else {
            event_id(ctx, store.as_ref(), &cals, &item.message.uid).await?
        };
        let n = Facts {
            id: item.id.to_string(),
            created: item.received_at,
            kind: kind(item, &message_event(item)),
            event,
        };
        if filter.map_or(Ok(true), |f| matches(f, &n))? {
            found.push(n);
        }
    }
    found.sort_by(|x, y| x.created.cmp(&y.created).then_with(|| x.id.cmp(&y.id)));
    if descending {
        found.reverse();
    }
    let ids: Vec<String> = found.into_iter().map(|n| n.id).collect();
    let mut out = object(json!({
        "accountId": a.account_id,
        "queryState": state_string(inbox.state),
        "canCalculateChanges": false,
    }));
    apply_window(&ids, &a.window, ctx.jmap.cfg.max_query_limit, &mut out)?;
    Ok(out)
}

/// `CalendarEventNotification/set`: только удаление (прочитанное уведомление); создание и изменение — `forbidden`.
pub(crate) async fn notification_set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: SetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    a.check_limit(ctx.jmap.cfg.max_objects_in_set)?;
    let store = ctx.jmap.dav_store()?.clone();
    let inbox = store.inbox_list(ctx.scope).await?;
    let old = state_string(inbox.state);
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }
    let mut out = SetOutcome::default();
    let forbidden = || {
        set_error(
            "forbidden",
            Some("notifications are created by the server"),
            &[],
        )
    };
    for (cid, _) in a.create.unwrap_or_default() {
        out.not_created.insert(cid, forbidden());
    }
    for (id, _) in a.update.unwrap_or_default() {
        out.not_updated.insert(id, forbidden());
    }
    for id in a.destroy.unwrap_or_default() {
        let oid = DavObjectId::from_str(ctx.resolve_id(&id)).ok();
        let Some(item) = inbox.items.iter().find(|i| Some(i.id) == oid) else {
            out.not_destroyed
                .insert(id, set_error("notFound", None, &[]));
            continue;
        };
        match store.inbox_delete(ctx.scope, &item.name).await {
            Ok(_) => out.destroyed.push(id),
            Err(tp_contracts::Error::NotFound) => {
                out.not_destroyed
                    .insert(id, set_error("notFound", None, &[]));
            }
            Err(e) => return Err(e.into()),
        }
    }
    let new = state_string(store.inbox_state(ctx.scope).await?);
    Ok(out.response(&a.account_id, &old, &new))
}

/// Состояние `CalendarEventNotification` для push; хранилище без входящих планирования — `None`.
pub(crate) async fn notification_state(
    jmap: &Jmap,
    scope: tp_contracts::meta::AccountScope,
) -> Option<String> {
    let state = jmap.dav.as_ref()?.inbox_state(scope).await.ok()?;
    Some(state_string(state))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tp_contracts::auth::Principal;
    use tp_contracts::dav::{CollectionKind, DavObject, DavStore};
    use tp_contracts::meta::AccountScope;
    use tp_contracts::schedule::{BusyKind, ItipMethod};

    use crate::calendars::tests::{call, create_calendar};
    use crate::testenv::Env;

    fn scope(p: &Principal) -> AccountScope {
        AccountScope {
            tenant: p.tenant,
            account: p.account,
        }
    }

    /// Объект календарей учётной записи с UID `uid` — то, что отдаёт CalDAV.
    async fn stored(env: &Env, scope: AccountScope, uid: &str) -> Option<DavObject> {
        let cals = env
            .dav
            .list_collections(scope, CollectionKind::Calendar)
            .await
            .expect("calendars");
        for c in cals {
            let found = env.dav.object_by_uid(scope, c.id, uid).await.expect("uid");
            if found.is_some() {
                return found;
            }
        }
        None
    }

    fn meeting() -> Value {
        json!({
            "uid": "meet-1",
            "title": "Планёрка",
            "start": "2027-01-11T10:00:00",
            "timeZone": "Europe/Moscow",
            "duration": "PT1H",
            "organizerCalendarAddress": "mailto:user@example.org",
            "participants": {
                "o": {"@type": "Participant", "name": "Иван",
                      "calendarAddress": "mailto:user@example.org", "roles": {"owner": true}},
                "b": {"@type": "Participant", "name": "Борис",
                      "calendarAddress": "mailto:bob@example.org", "roles": {"attendee": true},
                      "participationStatus": "needs-action", "expectReply": true},
                "x": {"@type": "Participant", "name": "Внешний",
                      "calendarAddress": "mailto:guest@example.com", "roles": {"attendee": true},
                      "participationStatus": "needs-action", "expectReply": true},
            },
        })
    }

    /// Приёмка: приглашение из JMAP приходит участнику в календарь (CalDAV), внешнему — письмом; ответ участника
    /// возвращается организатору; отмена доходит до участника. По входящим — `CalendarEventNotification`.
    #[tokio::test]
    async fn invitation_reply_and_cancel() {
        let env = Env::new().await;
        let acc = env.account();
        create_calendar(&env, "Работа").await;
        let bob = env.user("bob@example.org").await;
        let bob_acc = bob.account.to_string();

        // Организатор создаёт встречу с рассылкой.
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc, "sendSchedulingMessages": true, "create": {"e": meeting()}}),
        )
        .await;
        let id = r["created"]["e"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{r}"))
            .to_owned();

        // Участник организации: событие в его календаре, статус — ожидает ответа.
        let invited = stored(&env, scope(&bob), "meet-1")
            .await
            .expect("приглашение у участника");
        assert!(invited.body.contains("ORGANIZER"), "{}", invited.body);
        assert!(
            invited.body.contains("PARTSTAT=NEEDS-ACTION"),
            "{}",
            invited.body
        );
        assert!(invited.index.schedule_tag.is_some());
        // Внешний участник: одно письмо iMIP с REQUEST.
        let sent = env.imip.sent();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].method, ItipMethod::Request);
        assert_eq!(sent[0].to, ["guest@example.com"]);
        assert_eq!(sent[0].from, "user@example.org");
        // У организатора: итоги рассылки и Schedule-Tag.
        let mine = stored(&env, env.scope, "meet-1")
            .await
            .expect("событие организатора");
        assert!(mine.body.contains("SCHEDULE-STATUS=1.2"), "{}", mine.body);
        assert!(mine.body.contains("SCHEDULE-STATUS=1.1"), "{}", mine.body);
        assert!(mine.index.schedule_tag.is_some());

        // Участник видит приглашение в JMAP: событие (не организатор) и уведомление.
        let g = env
            .call_as(
                &bob,
                "CalendarEvent/get",
                json!({"accountId": bob_acc, "properties": ["participants", "isOrigin"]}),
            )
            .await;
        let event = &g["list"][0];
        assert_eq!(event["isOrigin"], false, "{g}");
        let bob_id = event["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{g}"))
            .to_owned();
        let key = event["participants"]
            .as_object()
            .and_then(|m| {
                m.iter()
                    .find(|(_, p)| p["calendarAddress"] == "mailto:bob@example.org")
            })
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| panic!("{g}"));
        let n = env
            .call_as(
                &bob,
                "CalendarEventNotification/get",
                json!({"accountId": bob_acc}),
            )
            .await;
        let list = n["list"].as_array().unwrap_or_else(|| panic!("{n}"));
        assert_eq!(list.len(), 1, "{n}");
        assert_eq!(list[0]["type"], "created", "{n}");
        assert_eq!(list[0]["changedBy"]["email"], "user@example.org", "{n}");
        assert_eq!(list[0]["changedBy"]["principalId"], acc, "{n}");
        assert_eq!(list[0]["calendarEventId"], bob_id, "{n}");
        assert_eq!(list[0]["event"]["title"], "Планёрка", "{n}");

        // Ответ участника: смена своего participationStatus.
        let patch = format!("participants/{key}/participationStatus");
        let r = env
            .call_as(
                &bob,
                "CalendarEvent/set",
                json!({"accountId": bob_acc, "sendSchedulingMessages": true,
                       "update": {&bob_id: {patch: "accepted"}}}),
            )
            .await;
        assert!(r["updated"].get(&bob_id).is_some(), "{r}");
        let mine = stored(&env, env.scope, "meet-1")
            .await
            .expect("событие организатора");
        assert!(mine.body.contains("PARTSTAT=ACCEPTED"), "{}", mine.body);
        let g = call(
            &env,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": [id], "properties": ["participants", "isOrigin"]}),
        )
        .await;
        assert_eq!(g["list"][0]["isOrigin"], true, "{g}");
        let people = g["list"][0]["participants"]
            .as_object()
            .unwrap_or_else(|| panic!("{g}"));
        let b = people
            .values()
            .find(|p| p["calendarAddress"] == "mailto:bob@example.org")
            .unwrap_or_else(|| panic!("{g}"));
        assert_eq!(b["participationStatus"], "accepted", "{g}");
        // Письма внешнему от ответа участника нет.
        assert_eq!(env.imip.sent().len(), 1);

        // Уведомление организатору об ответе; `/changes` и удаление прочитанного.
        let n = call(
            &env,
            "CalendarEventNotification/get",
            json!({"accountId": acc}),
        )
        .await;
        let list = n["list"].as_array().unwrap_or_else(|| panic!("{n}"));
        assert_eq!(list.len(), 1, "{n}");
        assert_eq!(list[0]["type"], "updated", "{n}");
        assert_eq!(list[0]["changedBy"]["email"], "bob@example.org", "{n}");
        assert_eq!(list[0]["calendarEventId"], id, "{n}");
        let (state, nid) = (n["state"].clone(), list[0]["id"].clone());
        let c = call(
            &env,
            "CalendarEventNotification/changes",
            json!({"accountId": acc, "sinceState": state}),
        )
        .await;
        assert_eq!(c["newState"], state, "{c}");
        assert_eq!(c["created"], json!([]), "{c}");
        let s = call(
            &env,
            "CalendarEventNotification/set",
            json!({"accountId": acc, "destroy": [nid, "nope"], "create": {"n": {}}}),
        )
        .await;
        assert_eq!(s["destroyed"], json!([nid]), "{s}");
        assert_eq!(s["notDestroyed"]["nope"]["type"], "notFound", "{s}");
        assert_eq!(s["notCreated"]["n"]["type"], "forbidden", "{s}");
        assert_ne!(s["newState"], state, "{s}");
        let c = call(
            &env,
            "CalendarEventNotification/changes",
            json!({"accountId": acc, "sinceState": state}),
        )
        .await;
        assert_eq!(c["destroyed"], json!([nid]), "{c}");
        assert_eq!(c["newState"], s["newState"], "{c}");
        let n = call(
            &env,
            "CalendarEventNotification/get",
            json!({"accountId": acc}),
        )
        .await;
        assert_eq!(n["list"], json!([]), "{n}");

        // Отмена: организатор удаляет встречу — у участника она отменена, внешнему — письмо CANCEL.
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc, "sendSchedulingMessages": true, "destroy": [id]}),
        )
        .await;
        assert_eq!(r["destroyed"], json!([id]), "{r}");
        let cancelled = stored(&env, scope(&bob), "meet-1")
            .await
            .expect("событие участника");
        assert!(
            cancelled.body.contains("STATUS:CANCELLED"),
            "{}",
            cancelled.body
        );
        let sent = env.imip.sent();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[1].method, ItipMethod::Cancel);
    }

    /// `CalendarEventNotification/query` от имени `who` с аргументами `args` (кроме `accountId`).
    async fn query(env: &Env, who: &Principal, args: Value) -> Value {
        let mut a = json!({"accountId": who.account.to_string()});
        if let (Some(a), Value::Object(extra)) = (a.as_object_mut(), args) {
            a.extend(extra);
        }
        env.call_as(who, "CalendarEventNotification/query", a).await
    }

    /// Приёмка JMAP-42: новое приглашение видно в `CalendarEventNotification/changes` и `/query`; `created` — время
    /// поступления во входящие.
    #[tokio::test]
    async fn notification_changes_and_query() {
        let env = Env::new().await;
        create_calendar(&env, "Работа").await;
        let bob = env.user("bob@example.org").await;
        let bob_acc = bob.account.to_string();
        let n = env
            .call_as(
                &bob,
                "CalendarEventNotification/get",
                json!({"accountId": bob_acc}),
            )
            .await;
        let old = n["state"].clone();
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": env.account(), "sendSchedulingMessages": true,
                   "create": {"e": meeting()}}),
        )
        .await;
        assert!(r["created"]["e"]["id"].is_string(), "{r}");

        let c = env
            .call_as(
                &bob,
                "CalendarEventNotification/changes",
                json!({"accountId": bob_acc, "sinceState": old}),
            )
            .await;
        let created = c["created"].as_array().unwrap_or_else(|| panic!("{c}"));
        assert_eq!(created.len(), 1, "{c}");
        assert_eq!(c["destroyed"], json!([]), "{c}");
        assert_eq!(c["hasMoreChanges"], false, "{c}");
        let nid = created[0].clone();
        let c2 = env
            .call_as(
                &bob,
                "CalendarEventNotification/changes",
                json!({"accountId": bob_acc, "sinceState": c["newState"]}),
            )
            .await;
        assert_eq!(c2["created"], json!([]), "{c2}");

        let g = env
            .call_as(
                &bob,
                "CalendarEventNotification/get",
                json!({"accountId": bob_acc, "ids": [nid]}),
            )
            .await;
        let note = &g["list"][0];
        assert_eq!(g["state"], c["newState"], "{g}");
        let at = note["created"].as_str().unwrap_or_else(|| panic!("{g}"));
        assert!(at.starts_with("20") && at.ends_with('Z'), "{g}");
        let event_id = note["calendarEventId"].clone();
        assert!(event_id.is_string(), "{g}");

        let q = query(&env, &bob, json!({"calculateTotal": true})).await;
        assert_eq!(q["ids"], json!([nid]), "{q}");
        assert_eq!(q["total"], 1, "{q}");
        assert_eq!(q["queryState"], c["newState"], "{q}");
        assert_eq!(q["canCalculateChanges"], false, "{q}");
        let cases = [
            (json!({"type": "created"}), json!([nid])),
            (json!({"type": "updated"}), json!([])),
            (json!({"calendarEventIds": [event_id]}), json!([nid])),
            (json!({"calendarEventIds": ["nope"]}), json!([])),
            (json!({"after": at}), json!([nid])),
            (json!({"before": at}), json!([])),
            (
                json!({"after": "2000-01-01T00:00:00Z", "type": "created"}),
                json!([nid]),
            ),
            (
                json!({"operator": "NOT", "conditions": [{"calendarEventIds": [event_id]}]}),
                json!([]),
            ),
        ];
        for (filter, want) in cases {
            let q = query(&env, &bob, json!({"filter": filter})).await;
            assert_eq!(q["ids"], want, "{filter}: {q}");
        }
        let q = query(
            &env,
            &bob,
            json!({"sort": [{"property": "created", "isAscending": false}]}),
        )
        .await;
        assert_eq!(q["ids"], json!([nid]), "{q}");
        let q = query(&env, &bob, json!({"sort": [{"property": "title"}]})).await;
        assert_eq!(q["type"], "unsupportedSort", "{q}");
        let q = query(&env, &bob, json!({"filter": {"flavour": "x"}})).await;
        assert_eq!(q["type"], "unsupportedFilter", "{q}");
    }

    /// Без `sendSchedulingMessages` событие с участниками записывается, но никому не рассылается.
    #[tokio::test]
    async fn silent_without_flag() {
        let env = Env::new().await;
        create_calendar(&env, "Работа").await;
        let bob = env.user("bob@example.org").await;
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": env.account(), "create": {"e": meeting()}}),
        )
        .await;
        assert!(r["created"]["e"]["id"].is_string(), "{r}");
        assert!(stored(&env, scope(&bob), "meet-1").await.is_none());
        assert!(env.imip.sent().is_empty());
        let mine = stored(&env, env.scope, "meet-1").await.expect("событие");
        assert!(!mine.body.contains("SCHEDULE-STATUS"), "{}", mine.body);
    }

    /// Приёмка JMAP-40: индекс события из JMAP — тот же, что у записи по CalDAV (`tp_dav::index::for_write`):
    /// `Schedule-Tag` только у встреч, занятость неповторяющегося события, повторы — без индекса занятости.
    #[tokio::test]
    async fn index_matches_caldav_write() {
        let env = Env::new().await;
        create_calendar(&env, "Работа").await;
        let mut weekly = meeting();
        weekly["uid"] = json!("weekly-1");
        weekly["start"] = json!("2026-01-05T10:00:00");
        weekly["recurrenceRule"] = json!({"@type": "RecurrenceRule", "frequency": "weekly"});
        let mut tentative = meeting();
        tentative["uid"] = json!("tentative-1");
        tentative["status"] = json!("tentative");
        let args = json!({"accountId": env.account(), "create": {
            "m": meeting(),
            "w": weekly,
            "t": tentative,
            "p": {"uid": "plain-1", "title": "Личное", "start": "2027-01-11T10:00:00", "duration": "PT1H"},
            "f": {"uid": "free-1", "title": "Свободно", "start": "2027-01-12T10:00:00", "duration": "PT1H",
                  "freeBusyStatus": "free"},
        }});
        let r = call(&env, "CalendarEvent/set", args).await;
        assert!(r["notCreated"].is_null(), "{r}");
        let opts = env
            .jmap
            .cfg
            .calendars
            .index_options(time::OffsetDateTime::now_utc());
        for uid in ["meet-1", "weekly-1", "tentative-1", "plain-1", "free-1"] {
            let obj = stored(&env, env.scope, uid).await.expect("событие");
            let ical = calcard::icalendar::ICalendar::parse(&obj.body).expect("iCalendar");
            let caldav = tp_dav::index::for_write(&ical, &opts).expect("индекс");
            assert_eq!(obj.index, caldav, "{uid}: {}", obj.body);
        }
        let meet = stored(&env, env.scope, "meet-1").await.expect("встреча");
        assert!(meet.index.schedule_tag.is_some());
        assert_eq!(meet.index.busy.len(), 1, "{:?}", meet.index.busy);
        assert_eq!(meet.index.busy[0].kind, BusyKind::Busy);
        let tentative = stored(&env, env.scope, "tentative-1")
            .await
            .expect("встреча");
        assert_eq!(tentative.index.busy[0].kind, BusyKind::Tentative);
        let plain = stored(&env, env.scope, "plain-1").await.expect("событие");
        assert_eq!(plain.index.schedule_tag, None);
        assert_eq!(plain.index.busy.len(), 1);
        let free = stored(&env, env.scope, "free-1").await.expect("событие");
        assert!(free.index.busy.is_empty(), "{:?}", free.index.busy);
        // Повторы в индекс занятости не пишутся: их разворачивает хранилище при запросе (META-54).
        let weekly = stored(&env, env.scope, "weekly-1").await.expect("встреча");
        assert!(weekly.index.schedule_tag.is_some());
        assert!(weekly.index.recurring && weekly.index.busy.is_empty());
    }

    #[tokio::test]
    async fn participant_identity() {
        let env = Env::new().await;
        let acc = env.account();
        let g = call(&env, "ParticipantIdentity/get", json!({"accountId": acc})).await;
        let list = g["list"].as_array().unwrap_or_else(|| panic!("{g}"));
        assert_eq!(list.len(), 1, "{g}");
        assert_eq!(list[0]["calendarAddress"], "mailto:user@example.org", "{g}");
        assert_eq!(list[0]["isDefault"], true, "{g}");
        let id = list[0]["id"].clone();
        let g = call(
            &env,
            "ParticipantIdentity/get",
            json!({"accountId": acc, "ids": [id, "nope"], "properties": ["calendarAddress"]}),
        )
        .await;
        assert_eq!(
            g["list"],
            json!([{"id": id, "calendarAddress": "mailto:user@example.org"}]),
            "{g}"
        );
        assert_eq!(g["notFound"], json!(["nope"]), "{g}");
    }
}
