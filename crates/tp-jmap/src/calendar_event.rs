//! JMAP Calendars — события (JMAP-28): `CalendarEvent/get`, `/changes`, `/set`, `/copy` поверх [`DavStore`].
//!
//! Хранится iCalendar — тот же объект, что читает и пишет CalDAV; JSCalendar ↔ iCalendar — calcard (JSCalendar 2.0,
//! draft-ietf-calext-jscalendarbis: `recurrenceRule`, `calendarAddress`), индекс объекта — `tp_dav::index::for_write`,
//! как у CalDAV (`Schedule-Tag`, занятость с обрезкой; JMAP-40).
//! Объект JMAP — событие ресурса (главный `VEVENT` с исключениями в `recurrenceOverrides`); свойства `VCALENDAR`
//! и прочие компоненты ресурса при изменении из JMAP сохраняются.
//!
//! Вхождение повтора — синтетический `id` `<id события>_<recurrenceId без разделителей>` (его выдаёт
//! `CalendarEvent/query` с `expandRecurrences`): `/get` — событие с применённым исключением, `recurrenceId` и
//! `baseEventId`; `/set` `update` — запись в `recurrenceOverrides` события, `destroy` — исключение вхождения.
//! `utcStart`, `utcEnd` — только по запросу в `properties`; время без пояса — в поясе `timeZone` запроса, иначе
//! календаря, иначе UTC.
//!
//! Отличия от черновика (см. также `crate::calendars`):
//! - событие — в одном календаре (`maxCalendarsPerEvent` = 1): перенос сменой `calendarIds` не поддерживается
//!   (контракт не переносит объект с сохранением `id`);
//! - `isDraft` — всегда `false`; `isOrigin` — только по запросу в `properties`; `mayInviteSelf` и прочие права
//!   участников не поддерживаются; отправка приглашений (`sendSchedulingMessages`) — `crate::scheduling`;
//! - `useDefaultAlerts`: `/get` отдаёт в `alerts` оповещения календаря по умолчанию (`defaultAlertsWithTime`, у
//!   события `showWithoutTime` — `defaultAlertsWithoutTime`; нет — без `alerts`), собственные оповещения события
//!   хранятся в iCalendar как есть (JMAP-36);
//! - `utcStart`/`utcEnd` при записи, `recurrenceOverridesBefore`/`After`, `reduceParticipants` не поддерживаются;
//! - в аккаунте владельца общих календарей (JMAP-32) видны события календарей с правом чтения, меняются — с правом
//!   записи (иначе — `forbidden`); `sendSchedulingMessages` там — `forbidden`;
//! - `/copy` между аккаунтами не поддерживается: `fromAccountNotFound`;
//! - `VTIMEZONE` в записываемый iCalendar не добавляется: пояс — идентификатор IANA в `TZID` (RFC 7809).

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::str::FromStr;

use calcard::common::timezone::Tz;
use calcard::icalendar::{ICalendar, ICalendarComponentType, ICalendarEntry, ICalendarProperty};
use calcard::jscalendar::JSCalendar;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};
use tp_contracts::Error;
use tp_contracts::dav::{
    Collection, CollectionId, Component, DavObject, DavObjectId, DavStore, DeleteOutcome,
    ObjectIndex, Precondition, Put, PutOutcome,
};
use tp_contracts::meta::AccountScope;
use tp_dav::index::IndexOptions;

use crate::calendars::{KIND, default_calendar, state};
use crate::contact_card::{apply_patch, load, single_book};
use crate::contacts::store_error;
use crate::core::Ctx;
use crate::core::error::{MethodError, MethodResult, changes_error, set_error};
use crate::core::request::{
    ChangesArgs, GetArgs, SetArgs, SetOutcome, changes_response, map_changes, object, parse_args,
    parse_state, select,
};
use crate::email_get::utc_date;
use crate::scheduling::{self, Written};

/// Свойства JMAP объекта `CalendarEvent`, которых нет в JSCalendar.
const JMAP_PROPS: [&str; 7] = [
    "id",
    "calendarIds",
    "isDraft",
    "isOrigin",
    "utcStart",
    "utcEnd",
    "baseEventId",
];

/// Свойства, которые нельзя менять у отдельного вхождения.
const INSTANCE_FIXED: [&str; 12] = [
    "id",
    "baseEventId",
    "calendarIds",
    "isDraft",
    "uid",
    "@type",
    "recurrenceId",
    "recurrenceIdTimeZone",
    "recurrenceRule",
    "excludedRecurrenceRule",
    "recurrenceOverrides",
    "excluded",
];

/// Вызов calcard с перехватом паники: конвертер проверяет вход `debug_assert` (участник без `calendarAddress`).
fn guarded<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    catch_unwind(AssertUnwindSafe(f)).ok().flatten()
}

/// `Group` JSCalendar ресурса без первой записи типа `ty` (`Event`, `Task`) и сама запись. Неразбираемое тело или
/// нет такой записи — `None`.
pub(crate) fn split(body: &str, ty: &str) -> Option<(Map<String, Value>, Map<String, Value>)> {
    let v = guarded(|| {
        let js = ICalendar::parse(body)
            .ok()?
            .into_jscalendar::<String, String>();
        serde_json::to_value(js).ok()
    })?;
    let Value::Object(mut group) = v else {
        return None;
    };
    let entries = group.get_mut("entries")?.as_array_mut()?;
    let at = entries.iter().position(|e| e["@type"] == ty)?;
    let Value::Object(event) = entries.remove(at) else {
        return None;
    };
    Some((group, event))
}

/// Событие JMAP из объекта хранилища: JSCalendar из iCalendar и свойства JMAP (`id`, `calendarIds`, `isDraft`).
/// Неразбираемое тело (хранилище его не проверяет) — событие только с `uid` из индекса.
pub(crate) fn event_json(obj: &DavObject) -> Map<String, Value> {
    let mut m = split(&obj.body, "Event")
        .map(|(_, e)| e)
        .unwrap_or_default();
    m.entry("@type").or_insert_with(|| json!("Event"));
    m.entry("uid").or_insert_with(|| json!(obj.index.uid));
    m.insert("id".into(), json!(obj.id.to_string()));
    let mut cals = Map::new();
    cals.insert(obj.collection.to_string(), json!(true));
    m.insert("calendarIds".into(), Value::Object(cals));
    m.insert("isDraft".into(), json!(false));
    m
}

pub(crate) fn invalid(why: &str, props: &[&str]) -> Value {
    set_error("invalidProperties", Some(why), props)
}

/// iCalendar и индекс из `Group` ресурса и записи JSCalendar (`Event`, `Task`) без свойств JMAP; ошибка —
/// `invalidProperties`.
pub(crate) fn entry_ical(
    mut group: Map<String, Value>,
    entry: Map<String, Value>,
    opts: &IndexOptions,
) -> Result<(String, ObjectIndex), Value> {
    group.entry("@type").or_insert_with(|| json!("Group"));
    match group.get_mut("entries").and_then(Value::as_array_mut) {
        Some(entries) => entries.insert(0, Value::Object(entry)),
        None => {
            group.insert("entries".into(), json!([entry]));
        }
    }
    let text = Value::Object(group).to_string();
    let ical = guarded(|| {
        JSCalendar::<String, String>::parse(&text)
            .ok()?
            .into_icalendar()
    })
    .ok_or_else(|| invalid("JSCalendar cannot be converted to iCalendar", &[]))?;
    let index =
        tp_dav::index::for_write(&ical, opts).map_err(|e| invalid(&e.to_string(), &["uid"]))?;
    Ok((ical.to_string(), index))
}

/// iCalendar и индекс из `Group` ресурса и события JMAP (свойства JMAP отбрасываются); ошибка —
/// `invalidProperties`.
fn to_ical(
    group: Map<String, Value>,
    mut event: Map<String, Value>,
    opts: &IndexOptions,
) -> Result<(String, ObjectIndex), Value> {
    for p in JMAP_PROPS {
        event.remove(p);
    }
    let (body, index) = entry_ical(group, event, opts)?;
    if index.span.is_none() {
        let why = "invalid start, duration or recurrence";
        return Err(invalid(why, &["start", "duration", "recurrenceRule"]));
    }
    Ok((body, index))
}

// ---------- Время и вхождения ----------

/// `LocalDateTime` (`2026-01-06T09:00:00`) из секунд местного времени.
pub(crate) fn local_key(ts: i64) -> Option<String> {
    let t = OffsetDateTime::from_unix_timestamp(ts).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    ))
}

/// Поля `LocalDateTime`: год, месяц, день, час, минута, секунда.
pub(crate) fn local_parts(s: &str) -> Option<[u16; 6]> {
    let b = s.as_bytes();
    if b.len() != 19 || !s.is_ascii() {
        return None;
    }
    let seps = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')];
    if seps.iter().any(|(i, c)| b[*i] != *c) {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<u16>().ok();
    Some([
        n(0..4)?,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    ])
}

/// Секунды местного времени из `LocalDateTime`.
pub(crate) fn local_ts(s: &str) -> Option<i64> {
    let [y, mo, d, h, mi, sec] = local_parts(s)?;
    let lo = |v: u16| u8::try_from(v).ok();
    let date =
        Date::from_calendar_date(i32::from(y), Month::try_from(lo(mo)?).ok()?, lo(d)?).ok()?;
    let time = Time::from_hms(lo(h)?, lo(mi)?, lo(sec)?).ok()?;
    Some(
        PrimitiveDateTime::new(date, time)
            .assume_utc()
            .unix_timestamp(),
    )
}

/// Пояс IANA по имени; неизвестный — `None`.
pub(crate) fn tz_of(name: Option<&str>) -> Option<Tz> {
    name.and_then(|n| Tz::from_str(n).ok())
}

/// Вхождение события.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Instance {
    /// `recurrenceId` (местное время пояса события, секунды); `None` — событие без повторов.
    pub rid: Option<i64>,
    /// Начало и конец, UTC.
    pub start: i64,
    pub end: i64,
}

/// Вхождения событий объекта (не больше `max`): повторы, исключения `EXDATE` и `RECURRENCE-ID` — calcard;
/// время без пояса — в поясе `tz`.
pub(crate) fn instances(body: &str, tz: Tz, max: usize) -> Vec<Instance> {
    use ICalendarProperty as P;
    let Ok(ical) = ICalendar::parse(body) else {
        return Vec::new();
    };
    let at = |e: &ICalendarEntry| {
        let dt = e.values.first()?.as_partial_date_time()?;
        let zone = ical
            .build_tz_resolver()
            .with_default(tz)
            .resolve_or_default(e.tz_id());
        dt.to_date_time_with_tz(zone)
    };
    let events = || {
        ical.components
            .iter()
            .filter(|c| c.component_type == ICalendarComponentType::VEvent)
    };
    // Пояс главного события: в нём записан `recurrenceId` вхождений.
    let master_tz = events()
        .find(|c| c.property(&P::RecurrenceId).is_none())
        .and_then(|c| c.property(&P::Dtstart))
        .and_then(at)
        .map(|d| d.timezone());
    let mut out = Vec::new();
    for e in &ical.expand_dates(tz, max).events {
        let Some(comp) = usize::try_from(e.comp_id)
            .ok()
            .and_then(|i| ical.components.get(i))
            .filter(|c| c.component_type == ICalendarComponentType::VEvent)
        else {
            continue;
        };
        let rid = match comp.property(&P::RecurrenceId) {
            Some(r) => {
                let Some(d) = at(r) else { continue };
                let d = master_tz.map_or(d, |t| d.with_timezone(&t));
                Some(d.naive_local().and_utc().timestamp())
            }
            None if comp.is_recurrent() => Some(e.start.naive_local().and_utc().timestamp()),
            None => None,
        };
        let (start, end) = e.timestamps();
        out.push(Instance {
            rid,
            start,
            end: end.max(start),
        });
    }
    out
}

/// `id` вхождения: `<id события>_<recurrenceId без разделителей>`.
pub(crate) fn instance_id(base: &str, rid: i64) -> Option<String> {
    let key = local_key(rid)?.replace(['-', ':'], "");
    Some(format!("{base}_{key}"))
}

/// `id` события и `recurrenceId` вхождения (`LocalDateTime`) из `id` JMAP.
fn split_id(id: &str) -> Option<(DavObjectId, Option<String>)> {
    let (base, rid) = match id.split_once('_') {
        Some((b, r)) => (b, Some(r)),
        None => (id, None),
    };
    let oid = DavObjectId::from_str(base).ok()?;
    let Some(r) = rid else {
        return Some((oid, None));
    };
    if r.len() != 15 || !r.is_ascii() {
        return None;
    }
    let key = format!(
        "{}-{}-{}:{}:{}",
        &r[..4],
        &r[4..6],
        &r[6..11],
        &r[11..13],
        &r[13..15]
    );
    local_ts(&key).map(|_| (oid, Some(key)))
}

/// Вхождение `rid` события `event`: исключение применено, правила повтора убраны.
pub(crate) fn instance_json(event: &Map<String, Value>, rid: &str, id: &str) -> Map<String, Value> {
    let mut m = event.clone();
    let base = m.get("id").cloned().unwrap_or(Value::Null);
    let over = m
        .remove("recurrenceOverrides")
        .and_then(|mut o| o.get_mut(rid).map(Value::take));
    m.remove("recurrenceRule");
    m.remove("excludedRecurrenceRule");
    m.insert("start".into(), json!(rid));
    if let Some(Value::Object(patch)) = over {
        for (k, v) in patch {
            let mut one = Map::new();
            one.insert(k, v);
            // Патч по несуществующему пути пропускается.
            let _ = apply_patch(&mut m, &one);
        }
    }
    m.insert("recurrenceId".into(), json!(rid));
    if let Some(tz) = m.get("timeZone").filter(|t| t.is_string()).cloned() {
        m.insert("recurrenceIdTimeZone".into(), tz);
    }
    m.insert("id".into(), json!(id));
    m.insert("baseEventId".into(), base);
    m
}

/// События календарей `cals` (в порядке календарей и имён ресурсов).
pub(crate) async fn load_events(
    store: &dyn DavStore,
    scope: AccountScope,
    cals: &[CollectionId],
) -> MethodResult<Vec<DavObject>> {
    let mut all = load(store, scope, cals).await?;
    all.retain(|o| o.index.component == Component::Event);
    Ok(all)
}

/// Календари аккаунта вызова с правом чтения.
pub(crate) async fn calendar_ids(ctx: &Ctx<'_>) -> MethodResult<Vec<CollectionId>> {
    Ok(ctx
        .jmap
        .dav_store()?
        .list_collections(ctx.scope, KIND)
        .await?
        .into_iter()
        .map(|c| c.id)
        .filter(|c| ctx.access.reads(*c))
        .collect())
}

/// Объект события по `id`; нет объекта, объект не событие (задача, карточка) или календарь без права чтения —
/// `None`.
async fn find(ctx: &Ctx<'_>, id: DavObjectId) -> MethodResult<Option<DavObject>> {
    let store = ctx.jmap.dav_store()?;
    let obj = store
        .objects_by_id(ctx.scope, &[id])
        .await?
        .into_iter()
        .next();
    Ok(obj.filter(|o| o.index.component == Component::Event && ctx.access.reads(o.collection)))
}

/// Ошибка `/set` записи в календарь без права записи.
fn forbidden() -> Value {
    set_error("forbidden", Some("mayWriteAll is required"), &[])
}

/// Свойства календарей событий запроса: пояс по умолчанию (время событий без пояса) и оповещения по умолчанию.
#[derive(Default)]
pub(crate) struct Zones {
    /// Пояс запроса (`timeZone`): важнее пояса календаря.
    pub request: Option<Tz>,
    /// Прочитанные календари; `None` — календаря нет.
    known: HashMap<CollectionId, Option<Collection>>,
}

impl Zones {
    pub(crate) fn new(request: Option<Tz>) -> Self {
        Self {
            request,
            known: HashMap::new(),
        }
    }

    /// Календарь `cal` (один запрос к хранилищу на календарь); нет — `None`.
    async fn calendar(
        &mut self,
        store: &dyn DavStore,
        scope: AccountScope,
        cal: CollectionId,
    ) -> MethodResult<Option<&Collection>> {
        if let std::collections::hash_map::Entry::Vacant(e) = self.known.entry(cal) {
            e.insert(store.get_collection(scope, cal).await?);
        }
        Ok(self.known.get(&cal).and_then(Option::as_ref))
    }

    /// Пояс для времени без пояса в календаре `cal`: запроса, календаря или UTC.
    pub(crate) async fn of(
        &mut self,
        store: &dyn DavStore,
        scope: AccountScope,
        cal: CollectionId,
    ) -> MethodResult<Tz> {
        if let Some(tz) = self.request {
            return Ok(tz);
        }
        let c = self.calendar(store, scope, cal).await?;
        Ok(c.and_then(|c| tz_of(c.timezone.as_deref()))
            .unwrap_or(Tz::UTC))
    }

    /// `useDefaultAlerts` (JMAP-36): `alerts` события `event` календаря `cal` — оповещения календаря по умолчанию
    /// (`defaultAlertsWithoutTime` для события `showWithoutTime`, иначе `defaultAlertsWithTime`); нет — без `alerts`.
    /// Событие без `useDefaultAlerts` не меняется.
    async fn default_alerts(
        &mut self,
        store: &dyn DavStore,
        scope: AccountScope,
        cal: CollectionId,
        event: &mut Map<String, Value>,
    ) -> MethodResult<()> {
        if event.get("useDefaultAlerts") != Some(&Value::Bool(true)) {
            return Ok(());
        }
        let all_day = event.get("showWithoutTime") == Some(&Value::Bool(true));
        let alerts = self.calendar(store, scope, cal).await?.and_then(|c| {
            let text = if all_day {
                c.alerts_without_time.as_deref()
            } else {
                c.alerts_with_time.as_deref()
            };
            serde_json::from_str::<Value>(text?).ok()
        });
        match alerts {
            Some(a) => event.insert("alerts".into(), a),
            None => event.remove("alerts"),
        };
        Ok(())
    }
}

/// `timeZone` запроса: пояс IANA; неизвестный — `invalidArguments`.
pub(crate) fn request_tz(v: Option<&Value>) -> MethodResult<Option<Tz>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => tz_of(Some(s))
            .map(Some)
            .ok_or_else(|| MethodError::invalid("timeZone: unknown time zone")),
        Some(_) => Err(MethodError::invalid("timeZone must be a string")),
    }
}

fn utc(ts: i64) -> Value {
    OffsetDateTime::from_unix_timestamp(ts).map_or(Value::Null, utc_date)
}

/// Событие или вхождение по `id` JMAP с `utcStart`/`utcEnd` (при `with_utc`); нет — `None`.
async fn get_one(
    ctx: &Ctx<'_>,
    zones: &mut Zones,
    id: &str,
    with_utc: bool,
    with_origin: bool,
) -> MethodResult<Option<Map<String, Value>>> {
    let store = ctx.jmap.dav_store()?;
    let Some((oid, rid)) = split_id(id) else {
        return Ok(None);
    };
    let Some(obj) = find(ctx, oid).await? else {
        return Ok(None);
    };
    let mut event = event_json(&obj);
    zones
        .default_alerts(store.as_ref(), ctx.scope, obj.collection, &mut event)
        .await?;
    if with_origin {
        let origin = scheduling::is_origin(&obj.body, &scheduling::addresses(ctx));
        event.insert("isOrigin".into(), json!(origin));
    }
    if rid.is_none() && !with_utc {
        return Ok(Some(event));
    }
    let tz = zones.of(store.as_ref(), ctx.scope, obj.collection).await?;
    let all = instances(&obj.body, tz, ctx.jmap.cfg.calendars.max_instances);
    let (mut m, inst) = match &rid {
        Some(key) => {
            let want = local_ts(key);
            let Some(inst) = all.iter().find(|i| i.rid == want) else {
                return Ok(None);
            };
            (instance_json(&event, key, id), Some(*inst))
        }
        None => (event, all.iter().min_by_key(|i| i.start).copied()),
    };
    if with_utc {
        m.insert(
            "utcStart".into(),
            inst.map_or(Value::Null, |i| utc(i.start)),
        );
        m.insert("utcEnd".into(), inst.map_or(Value::Null, |i| utc(i.end)));
    }
    Ok(Some(m))
}

/// Дополнительные аргументы `CalendarEvent/get`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GetExtra {
    time_zone: Option<Value>,
    #[serde(default)]
    reduce_participants: bool,
    recurrence_overrides_before: Option<Value>,
    recurrence_overrides_after: Option<Value>,
}

const GET_EXTRA: [&str; 4] = [
    "timeZone",
    "reduceParticipants",
    "recurrenceOverridesBefore",
    "recurrenceOverridesAfter",
];

/// `CalendarEvent/get`.
pub(crate) async fn get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let mut rest = args.clone();
    rest.retain(|k, _| GET_EXTRA.contains(&k.as_str()));
    let extra: GetExtra = parse_args(rest)?;
    let mut base = args;
    base.retain(|k, _| !GET_EXTRA.contains(&k.as_str()));
    let a: GetArgs = parse_args(base)?;
    ctx.open_dav(&a.account_id).await?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let unsupported = [
        extra.recurrence_overrides_before.as_ref(),
        extra.recurrence_overrides_after.as_ref(),
    ];
    if extra.reduce_participants || unsupported.iter().flatten().any(|v| !v.is_null()) {
        return Err(MethodError::invalid(
            "reduceParticipants and recurrenceOverridesBefore/After are not supported",
        ));
    }
    let mut zones = Zones::new(request_tz(extra.time_zone.as_ref())?);
    let store = ctx.jmap.dav_store()?;
    let st = state(store.as_ref(), ctx.scope).await?;
    let props = a.properties.as_deref();
    let with_utc = props.is_some_and(|p| p.iter().any(|x| x == "utcStart" || x == "utcEnd"));
    let with_origin = props.is_some_and(|p| p.iter().any(|x| x == "isOrigin"));
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    let ids: Vec<String> = match &a.ids {
        Some(ids) => ids.iter().map(|i| ctx.resolve_id(i).to_owned()).collect(),
        None => {
            let cals = calendar_ids(ctx).await?;
            let all = load_events(store.as_ref(), ctx.scope, &cals).await?;
            all.iter().map(|o| o.id.to_string()).collect()
        }
    };
    let mut seen: Vec<&str> = Vec::new();
    for id in &ids {
        if seen.contains(&id.as_str()) {
            continue;
        }
        seen.push(id);
        match get_one(ctx, &mut zones, id, with_utc, with_origin).await? {
            Some(m) => list.push(select(m, props)),
            None => not_found.push(id.clone()),
        }
    }
    Ok(object(json!({
        "accountId": a.account_id,
        "state": st,
        "list": list,
        "notFound": not_found,
    })))
}

/// `CalendarEvent/changes` по журналу объектов календарей: созданные и изменённые — только события календарей с
/// правом чтения (задачи `VTODO` того же журнала — `Task/changes`); удалённые — все удалённые объекты.
pub(crate) async fn changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: ChangesArgs = parse_args(args)?;
    ctx.open_dav(&a.account_id).await?;
    let limit = a.limit(ctx.jmap.cfg.max_changes)?;
    let since = parse_state(&a.since_state)?;
    let store = ctx.jmap.dav_store()?;
    let c = store
        .account_changes(ctx.scope, KIND, since, limit)
        .await
        .map_err(changes_error)?;
    let mut c = map_changes(c, ToString::to_string);
    crate::sharing::retain_objects(ctx, store.as_ref(), &mut c, |o| {
        o.index.component == Component::Event && ctx.access.reads(o.collection)
    })
    .await?;
    Ok(changes_response(&a.account_id, &a.since_state, &c))
}

// ---------- Изменение ----------

/// Итог записи как ошибка `/set`; `Ok` — записано.
fn put_error(r: tp_contracts::Result<PutOutcome>) -> MethodResult<Result<Put, Value>> {
    Ok(match r {
        Ok(PutOutcome::Stored(p)) => Ok(p),
        Ok(PutOutcome::UidConflict { .. }) => Err(invalid(
            "uid is used by another event of the calendar",
            &["uid"],
        )),
        Ok(PutOutcome::PreconditionFailed) => Err(set_error(
            "serverFail",
            Some("the event was changed concurrently"),
            &[],
        )),
        Err(Error::NotFound) => Err(invalid("calendar not found", &["calendarIds"])),
        Err(e) => Err(store_error(e)?),
    })
}

pub(crate) fn index_options(ctx: &Ctx<'_>) -> IndexOptions {
    ctx.jmap
        .cfg
        .calendars
        .index_options(OffsetDateTime::now_utc())
}

/// Свойства, которые клиент не задаёт: `isDraft: true` и время в UTC.
fn unsupported_props(event: &Map<String, Value>) -> Option<Value> {
    let bad: Vec<&str> = ["utcStart", "utcEnd", "baseEventId", "isOrigin"]
        .into_iter()
        .filter(|p| event.contains_key(*p))
        .chain(
            event
                .get("isDraft")
                .is_some_and(|d| *d != Value::Bool(false))
                .then_some("isDraft"),
        )
        .collect();
    (!bad.is_empty()).then(|| set_error("invalidProperties", None, &bad))
}

async fn create_one(ctx: &Ctx<'_>, v: &Value, send: bool) -> MethodResult<Result<Value, Value>> {
    let store = ctx.jmap.dav_store()?;
    let Value::Object(src) = v else {
        return Ok(Err(set_error("invalidProperties", None, &[])));
    };
    let mut event = src.clone();
    if event.contains_key("id") {
        return Ok(Err(set_error("invalidProperties", None, &["id"])));
    }
    if let Some(e) = unsupported_props(&event) {
        return Ok(Err(e));
    }
    let cal = match event.get("calendarIds") {
        Some(v) => single_book(v).and_then(|id| CollectionId::from_str(ctx.resolve_id(id)).ok()),
        None => default_calendar(&store.list_collections(ctx.scope, KIND).await?),
    };
    let Some(cal) = cal else {
        let why = "exactly one existing calendar is required";
        return Ok(Err(invalid(why, &["calendarIds"])));
    };
    if !ctx.access.writes(cal) {
        return Ok(Err(forbidden()));
    }
    let now = utc_date(OffsetDateTime::now_utc());
    let object_id = DavObjectId::new();
    event.entry("@type").or_insert_with(|| json!("Event"));
    event
        .entry("uid")
        .or_insert_with(|| json!(object_id.to_string()));
    event.entry("created").or_insert_with(|| now.clone());
    event.insert("updated".into(), now.clone());
    let server_set = json!({ "uid": event["uid"], "created": event["created"], "updated": now });
    let group = object(json!({ "@type": "Group", "prodId": "-//The Post//JMAP//RU" }));
    let (body, index) = match to_ical(group, event, &index_options(ctx)) {
        Ok(x) => x,
        Err(e) => return Ok(Err(e)),
    };
    let name = format!("{object_id}.ics");
    let put = store
        .put_object(
            ctx.scope,
            cal,
            &name,
            body.clone(),
            index.clone(),
            Precondition::IfNoneMatch,
        )
        .await;
    let put = match put_error(put)? {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    if send {
        let written = Written {
            collection: cal,
            name: &name,
            body: &body,
            index,
            etag: put.etag,
        };
        scheduling::announce(ctx, None, Some(written)).await;
    }
    Ok(Ok({
        let mut created = server_set;
        created["id"] = json!(put.id.to_string());
        created["isDraft"] = json!(false);
        for key in ["uid", "created"] {
            if src.contains_key(key) {
                created.as_object_mut().map(|m| m.remove(key));
            }
        }
        created
    }))
}

/// Изменение вхождения: патч или исключение — в `recurrenceOverrides` события.
fn patch_instance(
    event: &mut Map<String, Value>,
    rid: &str,
    patch: Option<&Map<String, Value>>,
) -> Result<(), Value> {
    let snapshot = event.clone();
    let overrides = event
        .entry("recurrenceOverrides")
        .or_insert_with(|| json!({}));
    let Value::Object(overrides) = overrides else {
        return Err(set_error("invalidPatch", Some("recurrenceOverrides"), &[]));
    };
    let Some(patch) = patch else {
        overrides.insert(rid.to_owned(), json!({ "excluded": true }));
        return Ok(());
    };
    let bad: Vec<&str> = patch
        .keys()
        .map(String::as_str)
        .filter(|k| INSTANCE_FIXED.contains(&k.split('/').next().unwrap_or_default()))
        .collect();
    if !bad.is_empty() {
        return Err(set_error("invalidProperties", None, &bad));
    }
    // Патч применяется к вхождению; в исключение записываются изменённые свойства целиком.
    let mut view = instance_json(&snapshot, rid, "");
    apply_patch(&mut view, patch)?;
    let over = overrides.entry(rid).or_insert_with(|| json!({}));
    let Value::Object(over) = over else {
        return Err(set_error("invalidPatch", Some("recurrenceOverrides"), &[]));
    };
    for key in patch.keys() {
        let top = key.split('/').next().unwrap_or_default();
        let nested = format!("{top}/");
        over.retain(|k, _| !k.starts_with(&nested));
        over.insert(
            top.to_owned(),
            view.get(top).cloned().unwrap_or(Value::Null),
        );
    }
    Ok(())
}

/// Изменяет событие `id`: `patch` — патч `/set`; `None` — удаление вхождения (только для `id` вхождения).
async fn update_one(
    ctx: &Ctx<'_>,
    id: &str,
    patch: Option<&Value>,
    send: bool,
) -> MethodResult<Result<Value, Value>> {
    let store = ctx.jmap.dav_store()?;
    let not_found = || Ok(Err(set_error("notFound", None, &[])));
    let Some((oid, rid)) = split_id(id) else {
        return not_found();
    };
    let Some(obj) = find(ctx, oid).await? else {
        return not_found();
    };
    if !ctx.access.writes(obj.collection) {
        return Ok(Err(forbidden()));
    }
    let patch = match patch {
        Some(Value::Object(p)) => Some(p),
        Some(_) => return Ok(Err(set_error("invalidPatch", None, &[]))),
        None => None,
    };
    let (group, _) = split(&obj.body, "Event").unwrap_or_default();
    let before = event_json(&obj);
    let mut event = before.clone();
    match &rid {
        Some(key) => {
            let want = local_ts(key);
            let max = ctx.jmap.cfg.calendars.max_instances;
            if !instances(&obj.body, Tz::UTC, max)
                .iter()
                .any(|i| i.rid == want)
            {
                return not_found();
            }
            if let Err(e) = patch_instance(&mut event, key, patch) {
                return Ok(Err(e));
            }
        }
        None => {
            let Some(patch) = patch else {
                return not_found();
            };
            if let Err(e) = apply_patch(&mut event, patch) {
                return Ok(Err(e));
            }
            if event.get("id") != before.get("id") {
                return Ok(Err(set_error("invalidProperties", None, &["id"])));
            }
            if event.get("uid") != before.get("uid") {
                return Ok(Err(invalid("uid cannot be changed", &["uid"])));
            }
            if let Some(e) = unsupported_props(&event) {
                return Ok(Err(e));
            }
            let cal = obj.collection.to_string();
            if event.get("calendarIds").and_then(single_book) != Some(cal.as_str()) {
                let why = "moving an event to another calendar is not supported";
                return Ok(Err(invalid(why, &["calendarIds"])));
            }
        }
    }
    let now = utc_date(OffsetDateTime::now_utc());
    event.insert("updated".into(), now.clone());
    let (body, index) = match to_ical(group, event, &index_options(ctx)) {
        Ok(x) => x,
        Err(e) => return Ok(Err(e)),
    };
    let pre = Precondition::IfMatch(obj.etag.clone());
    let put = store
        .put_object(
            ctx.scope,
            obj.collection,
            &obj.name,
            body.clone(),
            index.clone(),
            pre,
        )
        .await;
    let put = match put_error(put)? {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    if send {
        let written = Written {
            collection: obj.collection,
            name: &obj.name,
            body: &body,
            index,
            etag: put.etag,
        };
        scheduling::announce(ctx, Some(&obj.body), Some(written)).await;
    }
    Ok(Ok(if patch.is_some_and(|p| p.contains_key("updated")) {
        json!({ "updated": now })
    } else {
        Value::Null
    }))
}

/// Аргументы `CalendarEvent/set` сверх RFC 8620.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetExtra {
    #[serde(default)]
    send_scheduling_messages: bool,
}

/// `CalendarEvent/set`.
pub(crate) async fn set(
    ctx: &mut Ctx<'_>,
    mut args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let mut rest = Map::new();
    if let Some(v) = args.remove("sendSchedulingMessages") {
        rest.insert("sendSchedulingMessages".into(), v);
    }
    let extra: SetExtra = parse_args(rest)?;
    let send = extra.send_scheduling_messages;
    if send && ctx.jmap.schedule.is_none() {
        return Err(MethodError::invalid(
            "sendSchedulingMessages is not supported",
        ));
    }
    let a: SetArgs = parse_args(args)?;
    ctx.open_dav(&a.account_id).await?;
    if send && !ctx.access.is_own() {
        return Err(MethodError::with(
            "forbidden",
            "sendSchedulingMessages is not allowed in a shared account",
        ));
    }
    a.check_limit(ctx.jmap.cfg.max_objects_in_set)?;
    let store = ctx.jmap.dav_store()?.clone();
    let old = state(store.as_ref(), ctx.scope).await?;
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }
    let mut out = SetOutcome::default();
    for (cid, v) in a.create.unwrap_or_default() {
        match create_one(ctx, &v, send).await? {
            Ok(created) => {
                if let Some(id) = created["id"].as_str() {
                    ctx.created.insert(cid.clone(), id.to_owned());
                }
                out.created.insert(cid, created);
            }
            Err(e) => {
                out.not_created.insert(cid, e);
            }
        }
    }
    for (id, patch) in a.update.unwrap_or_default() {
        let rid = ctx.resolve_id(&id).to_owned();
        match update_one(ctx, &rid, Some(&patch), send).await? {
            Ok(v) => out.updated.insert(id, v),
            Err(e) => out.not_updated.insert(id, e),
        };
    }
    for id in a.destroy.unwrap_or_default() {
        let rid = ctx.resolve_id(&id).to_owned();
        let not_found = set_error("notFound", None, &[]);
        let Some((oid, inst)) = split_id(&rid) else {
            out.not_destroyed.insert(id, not_found);
            continue;
        };
        if inst.is_some() {
            // Удаление вхождения — исключение в событии.
            match update_one(ctx, &rid, None, send).await? {
                Ok(_) => out.destroyed.push(id),
                Err(e) => {
                    out.not_destroyed.insert(id, e);
                }
            }
            continue;
        }
        let Some(obj) = find(ctx, oid).await? else {
            out.not_destroyed.insert(id, not_found);
            continue;
        };
        if !ctx.access.writes(obj.collection) {
            out.not_destroyed.insert(id, forbidden());
            continue;
        }
        match store
            .delete_object(ctx.scope, obj.collection, &obj.name, Precondition::Any)
            .await
        {
            Ok(DeleteOutcome::Deleted(_)) => {
                if send {
                    scheduling::announce(ctx, Some(&obj.body), None).await;
                }
                out.destroyed.push(id);
            }
            Ok(DeleteOutcome::PreconditionFailed) | Err(Error::NotFound) => {
                out.not_destroyed.insert(id, not_found);
            }
            Err(e) => return Err(e.into()),
        }
    }
    let new = state(store.as_ref(), ctx.scope).await?;
    Ok(out.response(&a.account_id, &old, &new))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CopyArgs {
    from_account_id: String,
    account_id: String,
}

/// `CalendarEvent/copy`: копирование между аккаунтами не поддерживается — источник `fromAccountNotFound`.
pub(crate) async fn copy(
    ctx: &mut Ctx<'_>,
    mut args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    args.retain(|k, _| matches!(k.as_str(), "fromAccountId" | "accountId"));
    let a: CopyArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.jmap.dav_store()?;
    if a.from_account_id == a.account_id {
        return Err(MethodError::invalid(
            "fromAccountId and accountId must differ",
        ));
    }
    Err(MethodError::new("fromAccountNotFound"))
}

#[cfg(test)]
pub(crate) mod tests {
    use calcard::icalendar::{ICalendar, ICalendarComponentType, ICalendarProperty};
    use serde_json::{Value, json};
    use tp_contracts::dav::{DavStore, TimeRange};

    use crate::calendars::tests::{calendar_path, call, create_calendar, dav_request};
    use crate::contact_card::tests::dav;
    use crate::testenv::Env;

    /// VCALENDAR с компонентами.
    pub(crate) fn ical(components: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//RU\r\n{components}END:VCALENDAR\r\n"
        )
    }

    /// VEVENT с UID и свойствами `props` (строки с `\r\n`).
    pub(crate) fn vevent(uid: &str, props: &str) -> String {
        format!("BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20260101T000000Z\r\n{props}END:VEVENT\r\n")
    }

    /// Набор событий DAV-06 (`tp-dav`, `tests/query.rs`): имя ресурса и тело.
    pub(crate) fn dataset() -> Vec<(&'static str, String)> {
        vec![
            (
                "single.ics",
                ical(&vevent(
                    "ev-1",
                    "DTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\nSUMMARY:Планёрка\r\n",
                )),
            ),
            (
                // По вторникам (6, 13, 20, 27 января 09:00Z): 13-е исключено, 20-е перенесено на 21-е 15:00Z.
                "weekly.ics",
                ical(&format!(
                    "{}{}",
                    vevent(
                        "weekly",
                        "DTSTART:20260106T090000Z\r\nDTEND:20260106T100000Z\r\n\
                         RRULE:FREQ=WEEKLY;COUNT=4\r\nEXDATE:20260113T090000Z\r\nSUMMARY:Статус\r\n"
                    ),
                    vevent(
                        "weekly",
                        "RECURRENCE-ID:20260120T090000Z\r\nDTSTART:20260121T150000Z\r\n\
                         DTEND:20260121T160000Z\r\nSUMMARY:Перенос\r\n"
                    )
                )),
            ),
            (
                "allday.ics",
                ical(&vevent(
                    "allday",
                    "DTSTART;VALUE=DATE:20260110\r\nDTEND;VALUE=DATE:20260111\r\nSUMMARY:Выходной\r\n",
                )),
            ),
            (
                "moscow.ics",
                ical(&vevent(
                    "moscow",
                    "DTSTART;TZID=Europe/Moscow:20260112T100000\r\n\
                     DTEND;TZID=Europe/Moscow:20260112T110000\r\nSUMMARY:Встреча с заказчиком\r\n\
                     LOCATION:Переговорная 3\r\n\
                     ATTENDEE;PARTSTAT=NEEDS-ACTION;CN=Пётр:mailto:petr@example.org\r\n",
                )),
            ),
        ]
    }

    /// Календарь с набором событий, записанным CalDAV: адрес сервера DAV, `id` календаря, его путь.
    pub(crate) async fn put_dataset(env: &Env) -> (std::net::SocketAddr, String, String) {
        let addr = dav(env).await;
        let cal = create_calendar(env, "Работа").await;
        let base = calendar_path(env, &cal).await;
        for (name, body) in dataset() {
            let (status, text) = dav_request(addr, "PUT", &format!("{base}{name}"), &body).await;
            assert_eq!(status, 201, "PUT {name}: {text}");
        }
        (addr, cal, base)
    }

    /// События календаря по `uid`.
    pub(crate) async fn by_uid(env: &Env) -> std::collections::HashMap<String, Value> {
        let g = call(
            env,
            "CalendarEvent/get",
            json!({"accountId": env.account()}),
        )
        .await;
        g["list"]
            .as_array()
            .unwrap_or_else(|| panic!("{g}"))
            .iter()
            .map(|e| (e["uid"].as_str().expect("uid").to_owned(), e.clone()))
            .collect()
    }

    async fn create(env: &Env, event: Value) -> Value {
        let r = call(
            env,
            "CalendarEvent/set",
            json!({"accountId": env.account(), "create": {"e": event}}),
        )
        .await;
        assert!(r["created"]["e"]["id"].is_string(), "{r}");
        r["created"]["e"].clone()
    }

    /// Имя ресурса CalDAV события `id`.
    async fn resource(env: &Env, id: &str) -> String {
        let objs = env
            .dav
            .objects_by_id(env.scope, &[id.parse().expect("id")])
            .await
            .expect("objects");
        objs[0].name.clone()
    }

    /// Значение первого свойства `name` компонента `comp` как текст iCalendar.
    fn prop(ical: &ICalendar, comp: usize, name: &ICalendarProperty) -> Option<String> {
        let events: Vec<_> = ical
            .components
            .iter()
            .filter(|c| c.component_type == ICalendarComponentType::VEvent)
            .collect();
        let entry = events.get(comp)?.property(name)?;
        let mut line = String::new();
        entry.write_to(&mut line).ok()?;
        Some(line.replace("\r\n ", "").trim().to_owned())
    }

    /// События, записанные CalDAV, читаются JMAP: повтор с исключением, весь день, пояс, участники.
    #[tokio::test]
    async fn caldav_to_jmap() {
        let env = Env::new().await;
        let acc = env.account();
        let (addr, cal, base) = put_dataset(&env).await;
        let events = by_uid(&env).await;
        assert_eq!(events.len(), 4);

        let weekly = &events["weekly"];
        assert_eq!(weekly["@type"], "Event");
        assert_eq!(weekly["title"], "Статус");
        assert_eq!(weekly["start"], "2026-01-06T09:00:00");
        assert_eq!(weekly["timeZone"], "Etc/UTC");
        assert_eq!(weekly["duration"], "PT1H");
        assert_eq!(
            weekly["recurrenceRule"],
            json!({"frequency": "weekly", "count": 4})
        );
        let over = &weekly["recurrenceOverrides"];
        assert_eq!(
            over["2026-01-13T09:00:00"],
            json!({"excluded": true}),
            "{weekly}"
        );
        assert_eq!(over["2026-01-20T09:00:00"]["title"], "Перенос");
        assert_eq!(over["2026-01-20T09:00:00"]["start"], "2026-01-21T15:00:00");
        assert_eq!(weekly["calendarIds"], json!({&cal: true}));
        assert_eq!(weekly["isDraft"], false);

        let allday = &events["allday"];
        assert_eq!(allday["showWithoutTime"], true, "{allday}");
        assert_eq!(allday["start"], "2026-01-10T00:00:00");
        assert_eq!(allday["duration"], "P1D");
        assert!(allday.get("timeZone").is_none());

        let moscow = &events["moscow"];
        assert_eq!(moscow["timeZone"], "Europe/Moscow");
        assert_eq!(moscow["start"], "2026-01-12T10:00:00");
        let people: Vec<&Value> = moscow["participants"]
            .as_object()
            .expect("participants")
            .values()
            .collect();
        assert_eq!(people.len(), 1, "{moscow}");
        assert_eq!(people[0]["name"], "Пётр");
        assert_eq!(people[0]["calendarAddress"], "mailto:petr@example.org");
        assert_eq!(people[0]["participationStatus"], "needs-action");
        let place: Vec<&Value> = moscow["locations"]
            .as_object()
            .expect("locations")
            .values()
            .collect();
        assert_eq!(place[0]["name"], "Переговорная 3");

        // utcStart и utcEnd — по запросу; «весь день» — в поясе запроса или календаря.
        let id = moscow["id"].as_str().expect("id");
        let allday_id = allday["id"].as_str().expect("id");
        let g = call(
            &env,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": [id, allday_id], "properties": ["utcStart", "utcEnd"]}),
        )
        .await;
        assert_eq!(
            g["list"],
            json!([
                {"id": id, "utcStart": "2026-01-12T07:00:00Z", "utcEnd": "2026-01-12T08:00:00Z"},
                {"id": allday_id, "utcStart": "2026-01-10T00:00:00Z", "utcEnd": "2026-01-11T00:00:00Z"},
            ])
        );
        let g = call(
            &env,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": [allday_id], "properties": ["utcStart"],
                   "timeZone": "Europe/Moscow"}),
        )
        .await;
        assert_eq!(g["list"][0]["utcStart"], "2026-01-09T21:00:00Z", "{g}");

        // Правка из JMAP видна CalDAV, остальное содержимое ресурса сохраняется.
        let wid = weekly["id"].as_str().expect("id");
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc, "update": {wid: {"title": "Еженедельный статус",
                "recurrenceOverrides/2026-01-27T09:00:00": {"excluded": true}}}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        let (status, text) = dav_request(addr, "GET", &format!("{base}weekly.ics"), "").await;
        assert_eq!(status, 200, "{text}");
        let parsed = ICalendar::parse(&text).expect("ical");
        let body = text.replace("\r\n ", "");
        assert!(body.contains("SUMMARY:Еженедельный статус"), "{text}");
        assert!(body.contains("SUMMARY:Перенос"), "{text}");
        assert!(body.contains("RRULE:FREQ=WEEKLY;COUNT=4"), "{text}");
        assert!(body.contains("PRODID:-//test//RU"), "{text}");
        assert!(body.contains("RECURRENCE-ID:20260120T090000Z"), "{text}");
        let exdates: Vec<&str> = body.lines().filter(|l| l.starts_with("EXDATE")).collect();
        assert!(
            exdates.iter().any(|l| l.contains("20260113T090000Z")),
            "{text}"
        );
        assert!(
            exdates.iter().any(|l| l.contains("20260127T090000Z")),
            "{text}"
        );
        assert_eq!(parsed.uids().next(), Some("weekly"));
        // Индекс пересчитан: последнее вхождение теперь перенос 21 января.
        let late = TimeRange {
            start: time::OffsetDateTime::from_unix_timestamp(1_769_040_000).ok(),
            end: None,
        };
        let hits = env
            .dav
            .in_range(env.scope, cal.parse().expect("id"), late)
            .await
            .expect("range");
        assert!(hits.iter().all(|o| o.index.uid != "weekly"), "{hits:?}");
    }

    /// События, созданные JMAP, читаются CalDAV: повтор с исключением, весь день, участники, оповещения.
    #[tokio::test]
    async fn jmap_to_caldav() {
        let env = Env::new().await;
        let acc = env.account();
        let addr = dav(&env).await;
        let cal = create_calendar(&env, "Работа").await;
        let base = calendar_path(&env, &cal).await;

        let created = create(
            &env,
            json!({
                "calendarIds": {&cal: true}, "uid": "j1", "title": "Из JMAP", "description": "Текст",
                "start": "2026-02-03T10:00:00", "timeZone": "Europe/Moscow", "duration": "PT1H",
                "useDefaultAlerts": true,
                "recurrenceRule": {"frequency": "daily", "count": 3},
                "recurrenceOverrides": {
                    "2026-02-04T10:00:00": {"title": "Иначе", "start": "2026-02-04T12:00:00"},
                    "2026-02-05T10:00:00": {"excluded": true}},
                "organizerCalendarAddress": "mailto:ivan@example.org",
                "participants": {
                    "p0": {"@type": "Participant", "name": "Иван",
                           "calendarAddress": "mailto:ivan@example.org", "roles": {"owner": true}},
                    "p1": {"@type": "Participant", "name": "Пётр",
                           "calendarAddress": "mailto:petr@example.org", "roles": {"attendee": true},
                           "participationStatus": "needs-action", "expectReply": true}},
                "alerts": {"a1": {"@type": "Alert", "action": "display",
                                  "trigger": {"@type": "OffsetTrigger", "offset": "-PT10M"}}},
            }),
        )
        .await;
        let id = created["id"].as_str().expect("id").to_owned();
        assert!(created.get("uid").is_none(), "{created}");
        assert!(created["updated"].is_string());
        let day = create(
            &env,
            json!({"title": "День", "start": "2026-02-10T00:00:00", "duration": "P1D",
                   "showWithoutTime": true}),
        )
        .await;
        // Без `calendarIds` — календарь по умолчанию, `uid` задаёт сервер.
        let day_id = day["id"].as_str().expect("id");
        assert!(day["uid"].is_string(), "{day}");

        let name = resource(&env, &id).await;
        let day_name = resource(&env, day_id).await;
        let (status, text) = dav_request(addr, "GET", &format!("{base}{name}"), "").await;
        assert_eq!(status, 200, "{text}");
        let parsed = ICalendar::parse(&text).expect("ical");
        use ICalendarProperty as P;
        // Компоненты: исключение и главное событие (порядок — calcard).
        let (over, main) = if prop(&parsed, 0, &P::RecurrenceId).is_some() {
            (0, 1)
        } else {
            (1, 0)
        };
        assert_eq!(
            prop(&parsed, main, &P::Summary).as_deref(),
            Some("SUMMARY:Из JMAP")
        );
        assert_eq!(
            prop(&parsed, main, &P::Dtstart).as_deref(),
            Some("DTSTART;TZID=Europe/Moscow:20260203T100000")
        );
        assert_eq!(
            prop(&parsed, main, &P::Duration).as_deref(),
            Some("DURATION:PT1H")
        );
        assert_eq!(
            prop(&parsed, main, &P::Rrule).as_deref(),
            Some("RRULE:FREQ=DAILY;COUNT=3")
        );
        assert_eq!(
            prop(&parsed, main, &P::Exdate).as_deref(),
            Some("EXDATE;TZID=Europe/Moscow:20260205T100000")
        );
        assert_eq!(
            prop(&parsed, main, &P::Organizer).as_deref(),
            Some("ORGANIZER;CN=Иван;JSID=p0:mailto:ivan@example.org")
        );
        let attendee = prop(&parsed, main, &P::Attendee).expect("attendee");
        assert!(attendee.contains("PARTSTAT=NEEDS-ACTION"), "{attendee}");
        assert!(attendee.contains("RSVP=TRUE"), "{attendee}");
        assert!(attendee.ends_with("mailto:petr@example.org"), "{attendee}");
        assert_eq!(
            prop(&parsed, over, &P::RecurrenceId).as_deref(),
            Some("RECURRENCE-ID;TZID=Europe/Moscow:20260204T100000")
        );
        assert_eq!(
            prop(&parsed, over, &P::Summary).as_deref(),
            Some("SUMMARY:Иначе")
        );
        assert_eq!(
            prop(&parsed, over, &P::Dtstart).as_deref(),
            Some("DTSTART;TZID=Europe/Moscow:20260204T120000")
        );
        let body = text.replace("\r\n ", "");
        assert!(body.contains("BEGIN:VALARM"), "{text}");
        assert!(body.contains("TRIGGER:-PT10M"), "{text}");

        let (status, text) = dav_request(addr, "GET", &format!("{base}{day_name}"), "").await;
        assert_eq!(status, 200, "{text}");
        assert!(text.contains("DTSTART;VALUE=DATE:20260210"), "{text}");

        // CalDAV calendar-query по интервалу находит событие JMAP (индекс): 4 февраля — перенос на 12:00 MSK.
        let query = r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/></D:prop>
  <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT">
    <C:time-range start="20260204T090000Z" end="20260204T100000Z"/>
  </C:comp-filter></C:comp-filter></C:filter>
</C:calendar-query>"#;
        let (status, text) = dav_request(addr, "REPORT", &base, query).await;
        assert_eq!(status, 207, "{text}");
        assert!(text.contains(&name), "{text}");
        assert!(!text.contains(&day_name), "{text}");

        // Обратное чтение JMAP: то, что записал клиент.
        let g = call(
            &env,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": [id]}),
        )
        .await;
        let e = &g["list"][0];
        assert_eq!(e["title"], "Из JMAP", "{g}");
        assert_eq!(e["useDefaultAlerts"], true);
        assert_eq!(e["timeZone"], "Europe/Moscow");
        assert_eq!(
            e["recurrenceOverrides"]["2026-02-05T10:00:00"],
            json!({"excluded": true})
        );
        assert_eq!(
            e["recurrenceOverrides"]["2026-02-04T10:00:00"]["title"],
            "Иначе"
        );
        assert_eq!(e["participants"]["p1"]["expectReply"], true);
        // `useDefaultAlerts`: оповещения календаря (у него их нет), а не свои — те остаются в iCalendar.
        assert!(e.get("alerts").is_none(), "{g}");

        // Изменение CalDAV видно в JMAP: `id` тот же, меняется состояние.
        let state = g["state"].clone();
        let put = ical(&vevent(
            "j1",
            "DTSTART:20260301T080000Z\r\nDTEND:20260301T083000Z\r\nSUMMARY:Из Thunderbird\r\n",
        ));
        let (status, text) = dav_request(addr, "PUT", &format!("{base}{name}"), &put).await;
        assert_eq!(status, 204, "{text}");
        let c = call(
            &env,
            "CalendarEvent/changes",
            json!({"accountId": acc, "sinceState": state}),
        )
        .await;
        assert_eq!(c["updated"], json!([id]), "{c}");
        let g = call(
            &env,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": [id]}),
        )
        .await;
        assert_eq!(g["list"][0]["title"], "Из Thunderbird", "{g}");
        assert!(g["list"][0].get("recurrenceRule").is_none(), "{g}");
    }

    /// Вхождение повтора: чтение, изменение и удаление по синтетическому `id`.
    #[tokio::test]
    async fn instances_by_id() {
        let env = Env::new().await;
        let acc = env.account();
        put_dataset(&env).await;
        let weekly = by_uid(&env).await["weekly"].clone();
        let id = weekly["id"].as_str().expect("id");
        let inst = |d: &str| format!("{id}_{d}");

        let g = call(
            &env,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": [inst("20260106T090000"), inst("20260120T090000"),
                inst("20260113T090000"), inst("20260107T090000"), inst("garbage")],
                "properties": ["title", "start", "recurrenceId", "baseEventId", "utcStart", "utcEnd",
                    "recurrenceRule", "recurrenceOverrides"]}),
        )
        .await;
        assert_eq!(
            g["list"],
            json!([
                {"id": inst("20260106T090000"), "baseEventId": id, "title": "Статус",
                 "start": "2026-01-06T09:00:00", "recurrenceId": "2026-01-06T09:00:00",
                 "utcStart": "2026-01-06T09:00:00Z", "utcEnd": "2026-01-06T10:00:00Z"},
                {"id": inst("20260120T090000"), "baseEventId": id, "title": "Перенос",
                 "start": "2026-01-21T15:00:00", "recurrenceId": "2026-01-20T09:00:00",
                 "utcStart": "2026-01-21T15:00:00Z", "utcEnd": "2026-01-21T16:00:00Z"},
            ]),
            "{g}"
        );
        // Исключённое вхождение, дата вне правила и неверный `id` — `notFound`.
        assert_eq!(
            g["notFound"],
            json!([
                inst("20260113T090000"),
                inst("20260107T090000"),
                inst("garbage")
            ])
        );

        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc,
                "update": {
                    inst("20260106T090000"): {"title": "Первый",
                        "locations": {"l1": {"@type": "Location", "name": "Зал"}}},
                    inst("20260120T090000"): {"title": "Перенос 2"},
                    inst("20260127T090000"): {"recurrenceRule": null},
                    inst("20260107T090000"): {"title": "нет"}},
                "destroy": [inst("20260127T090000"), inst("20260113T090000")]}),
        )
        .await;
        assert!(r["updated"].get(inst("20260106T090000")).is_some(), "{r}");
        assert!(r["updated"].get(inst("20260120T090000")).is_some(), "{r}");
        assert_eq!(
            r["notUpdated"][inst("20260127T090000")]["type"],
            "invalidProperties",
            "{r}"
        );
        assert_eq!(r["notUpdated"][inst("20260107T090000")]["type"], "notFound");
        assert_eq!(r["destroyed"], json!([inst("20260127T090000")]), "{r}");
        assert_eq!(
            r["notDestroyed"][inst("20260113T090000")]["type"],
            "notFound"
        );

        let g = call(
            &env,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": [id]}),
        )
        .await;
        let over = &g["list"][0]["recurrenceOverrides"];
        assert_eq!(g["list"][0]["title"], "Статус", "{g}");
        assert_eq!(over["2026-01-06T09:00:00"]["title"], "Первый", "{g}");
        assert_eq!(over["2026-01-20T09:00:00"]["title"], "Перенос 2", "{g}");
        assert_eq!(
            over["2026-01-20T09:00:00"]["start"], "2026-01-21T15:00:00",
            "{g}"
        );
        assert_eq!(
            over["2026-01-27T09:00:00"],
            json!({"excluded": true}),
            "{g}"
        );
        let g = call(
            &env,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": [inst("20260106T090000"), inst("20260127T090000")],
                "properties": ["title", "locations"]}),
        )
        .await;
        assert_eq!(g["list"][0]["title"], "Первый", "{g}");
        let places: Vec<&Value> = g["list"][0]["locations"]
            .as_object()
            .expect("locations")
            .values()
            .collect();
        assert_eq!(places[0]["name"], "Зал", "{g}");
        assert_eq!(g["notFound"], json!([inst("20260127T090000")]));

        // Патч по пути внутрь свойства вхождения; путь через отсутствующее свойство — `invalidPatch`.
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc, "update": {
                inst("20260106T090000"): {"locations/l1/name": "Зал 2"},
                inst("20260120T090000"): {"locations/l1/name": "нет"}}}),
        )
        .await;
        assert!(r["updated"].get(inst("20260106T090000")).is_some(), "{r}");
        assert_eq!(
            r["notUpdated"][inst("20260120T090000")]["type"],
            "invalidPatch",
            "{r}"
        );
        let g = call(
            &env,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": [inst("20260106T090000")], "properties": ["title", "locations"]}),
        )
        .await;
        assert_eq!(g["list"][0]["title"], "Первый", "{g}");
        assert_eq!(g["list"][0]["locations"]["l1"]["name"], "Зал 2", "{g}");
    }

    #[tokio::test]
    async fn set_errors_changes_and_copy() {
        let env = Env::new().await;
        let acc = env.account();
        let cal = create_calendar(&env, "A").await;
        let other = create_calendar(&env, "B").await;
        let st = call(
            &env,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": []}),
        )
        .await["state"]
            .clone();
        let ok = json!({"calendarIds": {&cal: true}, "uid": "u1", "title": "T",
                        "start": "2026-03-01T10:00:00", "duration": "PT1H"});
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc, "ifInState": st, "create": {
                "ok": ok,
                "zdup": ok,
                "nostart": {"calendarIds": {&cal: true}, "title": "без начала"},
                "badstart": {"calendarIds": {&cal: true}, "start": "вчера"},
                "nocal": {"calendarIds": {"nope": true}, "start": "2026-03-01T10:00:00"},
                "twocal": {"calendarIds": {&cal: true, &other: true}, "start": "2026-03-01T10:00:00"},
                "draft": {"start": "2026-03-01T10:00:00", "isDraft": true, "utcStart": "2026-03-01T10:00:00Z"},
                "withid": {"id": "x", "start": "2026-03-01T10:00:00"},
                // Участник без адреса: calcard проверяет это `debug_assert` — запрос не падает.
                "noaddr": {"calendarIds": {&other: true}, "start": "2026-03-01T10:00:00",
                           "participants": {"p": {"@type": "Participant", "name": "Без адреса"}}},
            }}),
        )
        .await;
        let id = r["created"]["ok"]["id"].as_str().expect("id").to_owned();
        let not = &r["notCreated"];
        assert_eq!(not["zdup"]["properties"], json!(["uid"]), "{r}");
        for key in ["nostart", "badstart"] {
            assert_eq!(not[key]["type"], "invalidProperties", "{key}: {r}");
            assert_eq!(not[key]["properties"][0], "start", "{key}: {r}");
        }
        assert_eq!(not["nocal"]["properties"], json!(["calendarIds"]), "{r}");
        assert_eq!(not["twocal"]["properties"], json!(["calendarIds"]), "{r}");
        assert_eq!(
            not["draft"]["properties"],
            json!(["utcStart", "isDraft"]),
            "{r}"
        );
        assert_eq!(not["withid"]["properties"], json!(["id"]), "{r}");
        if let Some(e) = not.get("noaddr") {
            assert_eq!(e["type"], "invalidProperties", "{r}");
        }

        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc, "update": {
                &id: {"calendarIds": {&other: true}},
                "nope": {"title": "x"}}}),
        )
        .await;
        assert_eq!(
            r["notUpdated"][&id]["properties"],
            json!(["calendarIds"]),
            "{r}"
        );
        assert_eq!(r["notUpdated"]["nope"]["type"], "notFound", "{r}");
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc, "update": {&id: {"uid": "other"}}}),
        )
        .await;
        assert_eq!(r["notUpdated"][&id]["properties"], json!(["uid"]), "{r}");
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc, "update": {&id: {"title": "T2", "duration": "PT2H"}}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        // Планирование не подключено — рассылка недоступна; подключено — пустой запрос проходит.
        let plain = Env::unscheduled().await;
        let args = json!({"accountId": plain.account(), "sendSchedulingMessages": true});
        let r = call(&plain, "CalendarEvent/set", args).await;
        assert_eq!(r["type"], "invalidArguments", "{r}");
        let args = json!({"accountId": acc, "sendSchedulingMessages": true});
        let r = call(&env, "CalendarEvent/set", args).await;
        assert!(r["newState"].is_string(), "{r}");
        let r = call(
            &env,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": [id], "reduceParticipants": true}),
        )
        .await;
        assert_eq!(r["type"], "invalidArguments", "{r}");

        let c = call(
            &env,
            "CalendarEvent/changes",
            json!({"accountId": acc, "sinceState": st}),
        )
        .await;
        assert!(
            c["created"]
                .as_array()
                .is_some_and(|a| a.contains(&json!(id))),
            "{c}"
        );
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc, "destroy": [id, "nope"]}),
        )
        .await;
        assert_eq!(r["destroyed"], json!([id]), "{r}");
        assert_eq!(r["notDestroyed"]["nope"]["type"], "notFound", "{r}");
        let c = call(
            &env,
            "CalendarEvent/changes",
            json!({"accountId": acc, "sinceState": c["newState"]}),
        )
        .await;
        assert_eq!(c["destroyed"], json!([id]), "{c}");

        let r = call(
            &env,
            "CalendarEvent/copy",
            json!({"fromAccountId": "other", "accountId": acc, "create": {}}),
        )
        .await;
        assert_eq!(r["type"], "fromAccountNotFound", "{r}");
    }

    /// Задача (`VTODO`) календаря и карточка контактов — не события.
    #[tokio::test]
    async fn todos_are_not_events() {
        let env = Env::new().await;
        let acc = env.account();
        let addr = dav(&env).await;
        let cal = create_calendar(&env, "A").await;
        let base = calendar_path(&env, &cal).await;
        let todo = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//RU\r\nBEGIN:VTODO\r\nUID:t1\r\n\
                    DTSTAMP:20260101T000000Z\r\nSUMMARY:Задача\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let (status, text) = dav_request(addr, "PUT", &format!("{base}t1.ics"), todo).await;
        assert_eq!(status, 201, "{text}");
        let g = call(&env, "CalendarEvent/get", json!({"accountId": acc})).await;
        assert_eq!(g["list"], json!([]), "{g}");
        let todo_id = env
            .dav
            .list_objects(env.scope, cal.parse().expect("id"))
            .await
            .expect("list")[0]
            .id
            .to_string();
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc, "destroy": [todo_id]}),
        )
        .await;
        assert_eq!(r["notDestroyed"][&todo_id]["type"], "notFound", "{r}");
    }
}
