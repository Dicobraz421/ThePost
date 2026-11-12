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
//! ресурса входящих, `state` — `Inbox::state`. Отличия от черновика:
//! - уведомление — входящее сообщение iTIP, а не запись о чужой правке общего календаря (общие календари — JMAP-32);
//!   применённые сервером `REQUEST`, `REPLY`, `CANCEL` во входящих остаются только при `dav.schedule_keep_processed`;
//! - `type`: `CANCEL` — `destroyed`; `REQUEST`, `ADD`, `PUBLISH` с `sequence` 0 — `created`; остальное — `updated`;
//! - `event` — событие из сообщения (у `updated` — новое состояние, `eventPatch` — `null`: прежнего нет);
//! - `created` — отметка времени из сообщения (`DTSTAMP`): времени поступления во входящих нет;
//! - `/changes`: входящие без надгробий, поэтому при изменившемся состоянии — `cannotCalculateChanges`;
//! - `/query`, `/queryChanges` не поддерживаются;
//! - `ParticipantIdentity` — один адрес (имя входа); `/changes`, `/set` не поддерживаются.

use std::str::FromStr;

use calcard::icalendar::ICalendar;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tp_contracts::dav::{
    CollectionId, DavObjectId, DavStore, ObjectIndex, Precondition, PutOutcome,
};
use tp_contracts::schedule::{InboxItem, ItipMethod};
use tp_dav::index::IndexOptions;
use tp_dav::itip;

use crate::calendar_event::{calendar_ids, split};
use crate::core::error::{MethodError, MethodResult, set_error};
use crate::core::request::{
    ChangesArgs, GetArgs, SetArgs, SetOutcome, changes_response, object, parse_args, parse_state,
    select, state_string,
};
use crate::core::{Ctx, Jmap};

/// Адреса календарного пользователя вошедшего (`tp_dav::itip::address`): имя входа, если оно — адрес (как в CalDAV).
pub(crate) fn addresses(ctx: &Ctx<'_>) -> Vec<String> {
    let login = itip::address(&ctx.principal.login);
    if login.contains('@') {
        vec![login]
    } else {
        Vec::new()
    }
}

/// Индекс объекта календаря для записи: интервал, занятость (у повторяющегося события — признак
/// `ObjectIndex::recurring`, META-54), `Schedule-Tag` — как при записи CalDAV.
pub(crate) fn object_index(
    ical: &ICalendar,
    opts: &IndexOptions,
) -> Result<ObjectIndex, tp_dav::index::IndexError> {
    let mut idx = tp_dav::index::of_calendar(ical, opts)?;
    idx.schedule_tag = itip::schedule_tag(ical);
    Ok(idx)
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

/// `id` события календарей учётной записи с UID `uid`.
async fn event_id(ctx: &Ctx<'_>, store: &dyn DavStore, uid: &str) -> MethodResult<Option<String>> {
    for cal in calendar_ids(store, ctx.scope).await? {
        if let Some(o) = store.object_by_uid(ctx.scope, cal, uid).await? {
            return Ok(Some(o.id.to_string()));
        }
    }
    Ok(None)
}

/// Уведомление из сообщения входящих планирования.
async fn notification(
    ctx: &Ctx<'_>,
    store: &dyn DavStore,
    item: &InboxItem,
) -> MethodResult<Map<String, Value>> {
    let msg = &item.message;
    let mut event = split(&msg.body).map(|(_, e)| e).unwrap_or_default();
    event.entry("@type").or_insert_with(|| json!("Event"));
    event.entry("uid").or_insert_with(|| json!(msg.uid));
    let first = event.get("sequence").and_then(Value::as_u64).unwrap_or(0) == 0;
    let kind = match msg.method {
        ItipMethod::Cancel => "destroyed",
        ItipMethod::Request | ItipMethod::Add | ItipMethod::Publish if first => "created",
        _ => "updated",
    };
    let from = itip::address(&msg.from);
    let principal = ctx
        .jmap
        .d
        .accounts
        .by_login(ctx.scope.tenant, &from)
        .await?
        .map(|a| a.id.to_string());
    let created = ["updated", "created"]
        .iter()
        .find_map(|k| event.get(*k).filter(|v| v.is_string()).cloned())
        .unwrap_or_else(|| json!("1970-01-01T00:00:00Z"));
    Ok(object(json!({
        "id": item.id.to_string(),
        "created": created,
        "changedBy": {
            "name": participant_name(&event, &from).unwrap_or_default(),
            "email": from,
            "principalId": principal,
            "calendarAddress": format!("mailto:{from}"),
        },
        "comment": null,
        "type": kind,
        "calendarEventId": event_id(ctx, store, &msg.uid).await?,
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
    for item in picked {
        list.push(select(
            notification(ctx, store.as_ref(), item).await?,
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

/// `CalendarEventNotification/changes`: входящие без надгробий — при изменившемся состоянии клиент перечитывает
/// список (`cannotCalculateChanges`).
pub(crate) async fn notification_changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: ChangesArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    a.limit(ctx.jmap.cfg.max_changes)?;
    let since = parse_state(&a.since_state)?;
    let inbox = ctx.jmap.dav_store()?.inbox_list(ctx.scope).await?;
    if since != inbox.state {
        return Err(MethodError::new("cannotCalculateChanges"));
    }
    let none = tp_contracts::meta::Changes::<String> {
        created: Vec::new(),
        updated: Vec::new(),
        destroyed: Vec::new(),
        new_state: inbox.state,
        has_more: false,
    };
    Ok(changes_response(&a.account_id, &a.since_state, &none))
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
    let new = state_string(store.inbox_list(ctx.scope).await?.state);
    Ok(out.response(&a.account_id, &old, &new))
}

/// Состояние `CalendarEventNotification` для push; хранилище без входящих планирования — `None`.
pub(crate) async fn notification_state(
    jmap: &Jmap,
    scope: tp_contracts::meta::AccountScope,
) -> Option<String> {
    let inbox = jmap.dav.as_ref()?.inbox_list(scope).await.ok()?;
    Some(state_string(inbox.state))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tp_contracts::auth::Principal;
    use tp_contracts::dav::{CollectionKind, DavObject, DavStore};
    use tp_contracts::meta::AccountScope;
    use tp_contracts::schedule::ItipMethod;

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
        assert_eq!(c["type"], "cannotCalculateChanges", "{c}");
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

    /// Индекс события, записанного из JMAP: `Schedule-Tag` только у встреч, занятость — до горизонта хранилища.
    #[tokio::test]
    async fn index_has_schedule_tag_and_clipped_busy() {
        let env = Env::new().await;
        create_calendar(&env, "Работа").await;
        let mut event = meeting();
        event["start"] = json!("2026-01-05T10:00:00");
        event["recurrenceRule"] = json!({"@type": "RecurrenceRule", "frequency": "weekly"});
        let args = json!({"accountId": env.account(), "create": {
            "m": event,
            "p": {"uid": "plain-1", "title": "Личное", "start": "2027-01-11T10:00:00", "duration": "PT1H"},
        }});
        let r = call(&env, "CalendarEvent/set", args).await;
        assert!(r["notCreated"].is_null(), "{r}");
        let plain = stored(&env, env.scope, "plain-1").await.expect("событие");
        assert_eq!(plain.index.schedule_tag, None);
        let weekly = stored(&env, env.scope, "meet-1").await.expect("встреча");
        assert!(weekly.index.schedule_tag.is_some());
        // Повторы в индекс занятости не пишутся: их разворачивает хранилище при запросе (META-54).
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
