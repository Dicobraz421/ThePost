//! JMAP Calendars — запрос событий (JMAP-29): `CalendarEvent/query`, `/queryChanges` поверх [`DavStore`].
//!
//! Фильтр: `inCalendar` (`inCalendars` — список, как в ранних редакциях черновика), `after`, `before`, `text`,
//! `title`, `description`, `location`, `owner`, `attendee`, `uid`; операторы `AND`, `OR`, `NOT`. Сортировка —
//! `start`, `uid`, `created`, `updated`. Интервал — как `time-range` CalDAV (RFC 4791 §9.9): вхождение `[начало,
//! конец)` пересекает `[after, before)`, вхождение нулевой длины — по началу. `after` и `before` — `UTCDate` или
//! `LocalDateTime` в поясе `timeZone` запроса (иначе UTC); время событий без пояса и даты «весь день» — в поясе
//! `timeZone` запроса, иначе календаря, иначе UTC.
//!
//! Кандидаты по интервалу выбирает индекс объекта (`tp_dav::index`, [`DavStore::in_range`]), точную проверку
//! делает развёртка повторов calcard (исключения `EXDATE`, `RECURRENCE-ID`). `expandRecurrences` — каждое
//! вхождение интервала отдельным результатом с синтетическим `id` (`crate::calendar_event`); нужны обе границы
//! в условии верхнего уровня и интервал не длиннее `jmap.calendars.max_expanded_query_duration`.
//! `/queryChanges` — без `expandRecurrences` (`canCalculateChanges` запроса с ним — `false`).

use std::cmp::Ordering;
use std::str::FromStr;

use calcard::common::PartialDateTime;
use calcard::common::timezone::Tz;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tp_contracts::Error;
use tp_contracts::dav::{CollectionId, Component, DavObject, TimeRange};

use crate::calendar_event::{
    Instance, Zones, calendar_ids, event_json, instance_id, instance_json, instances, load_events,
    local_key, local_parts, request_tz,
};
use crate::calendars::{KIND, state};
use crate::contact_card::contains;
use crate::core::Ctx;
use crate::core::error::{MethodError, MethodResult, changes_error};
use crate::core::request::{
    Comparator, QueryChangesArgs, QueryWindow, apply_window, map_changes, object, parse_args,
    parse_state, query_changes_response,
};

/// Запас интервала кандидатов: индекс считает время без пояса и даты «весь день» как UTC, а точная проверка — в
/// поясе запроса или календаря (сдвиг до 14 часов).
const SKEW: i64 = 86_400;

/// Свойства сортировки.
const SORT_PROPS: [&str; 4] = ["start", "uid", "created", "updated"];

/// Свойства, по которым ищет `text`.
const TEXT_PROPS: [&str; 5] = [
    "title",
    "description",
    "locations",
    "participants",
    "recurrenceOverrides",
];

/// Результат запроса: событие или вхождение.
struct Item {
    id: String,
    json: Map<String, Value>,
    /// Вхождения, по которым проверяется интервал.
    instances: Vec<Instance>,
}

impl Item {
    fn start(&self) -> i64 {
        self.instances
            .iter()
            .map(|i| i.start)
            .min()
            .unwrap_or(i64::MAX)
    }

    fn text(&self, prop: &str) -> &str {
        self.json
            .get(prop)
            .and_then(Value::as_str)
            .unwrap_or_default()
    }
}

/// Граница интервала: `UTCDate` или `LocalDateTime` в поясе `tz`.
fn bound(v: &Value, tz: Tz) -> Option<i64> {
    let s = v.as_str()?;
    if let Ok(t) = OffsetDateTime::parse(s, &Rfc3339) {
        return Some(t.unix_timestamp());
    }
    let [y, mo, d, h, mi, sec] = local_parts(s)?;
    let lo = |v: u16| u8::try_from(v).ok();
    let dt = PartialDateTime {
        year: Some(y),
        month: lo(mo),
        day: lo(d),
        hour: lo(h),
        minute: lo(mi),
        second: lo(sec),
        ..Default::default()
    };
    Some(dt.to_date_time_with_tz(tz)?.timestamp())
}

/// Вхождение `[s, e)` пересекает `[after, before)`; нулевой длины — по началу (RFC 4791 §9.9).
fn overlaps(i: &Instance, after: Option<i64>, before: Option<i64>) -> bool {
    let (a, b) = (after.unwrap_or(i64::MIN), before.unwrap_or(i64::MAX));
    i.start < b && (i.end > a || (i.end == i.start && i.start >= a))
}

/// Участники события с ролью `role` содержат `needle` (имя, адреса).
fn participant(json: &Map<String, Value>, role: &str, needle: &str) -> bool {
    let Some(Value::Object(all)) = json.get("participants") else {
        return false;
    };
    all.values()
        .filter_map(Value::as_object)
        .filter(|p| match p.get("roles") {
            Some(Value::Object(r)) => r.get(role) == Some(&Value::Bool(true)),
            // Без ролей — участник (JSCalendar: роль по умолчанию — `attendee`).
            _ => role == "attendee",
        })
        .any(|p| contains(p, &["name", "calendarAddress", "email", "sendTo"], needle))
}

/// Проверка фильтра (`FilterOperator` или `FilterCondition`).
fn matches(filter: &Value, item: &Item, tz: Tz) -> MethodResult<bool> {
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
            results.push(matches(c, item, tz)?);
        }
        return match op.as_str() {
            Some("AND") => Ok(results.iter().all(|r| *r)),
            Some("OR") => Ok(results.iter().any(|r| *r)),
            Some("NOT") => Ok(!results.iter().any(|r| *r)),
            _ => Err(MethodError::invalid("unknown filter operator")),
        };
    }
    let mut ok = true;
    let (mut after, mut before) = (None, None);
    for (key, val) in f {
        let unsupported = || MethodError::with("unsupportedFilter", key.clone());
        let text = || val.as_str().map(str::to_lowercase).ok_or_else(unsupported);
        let in_calendar = |id: &str| {
            item.json
                .get("calendarIds")
                .and_then(|c| c.get(id))
                .is_some()
        };
        ok &= match key.as_str() {
            "after" => {
                after = Some(bound(val, tz).ok_or_else(unsupported)?);
                true
            }
            "before" => {
                before = Some(bound(val, tz).ok_or_else(unsupported)?);
                true
            }
            "inCalendar" => in_calendar(val.as_str().ok_or_else(unsupported)?),
            "inCalendars" => val
                .as_array()
                .ok_or_else(unsupported)?
                .iter()
                .any(|id| id.as_str().is_some_and(in_calendar)),
            "uid" => item.json.get("uid") == Some(val),
            "text" => contains(&item.json, &TEXT_PROPS, &text()?),
            "title" => contains(&item.json, &["title"], &text()?),
            "description" => contains(&item.json, &["description"], &text()?),
            "location" => contains(&item.json, &["locations"], &text()?),
            "owner" => participant(&item.json, "owner", &text()?),
            "attendee" => participant(&item.json, "attendee", &text()?),
            _ => return Err(unsupported()),
        };
    }
    if after.is_some() || before.is_some() {
        // Обе границы условия — по одному и тому же вхождению.
        ok &= item.instances.iter().any(|i| overlaps(i, after, before));
    }
    Ok(ok)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryArgs {
    account_id: String,
    filter: Option<Value>,
    sort: Option<Vec<Comparator>>,
    #[serde(default)]
    expand_recurrences: bool,
    time_zone: Option<Value>,
    #[serde(flatten)]
    window: QueryWindow,
}

/// Границы `after` и `before` условия верхнего уровня (не оператора).
fn top_bounds(filter: Option<&Value>, tz: Tz) -> (Option<i64>, Option<i64>) {
    let cond = filter.filter(|f| f.get("operator").is_none());
    let at = |key: &str| cond.and_then(|f| f.get(key)).and_then(|v| bound(v, tz));
    (at("after"), at("before"))
}

/// События-кандидаты календаря: по индексу интервала или все.
async fn candidates(
    ctx: &Ctx<'_>,
    cal: CollectionId,
    range: Option<TimeRange>,
) -> MethodResult<Vec<DavObject>> {
    let store = ctx.jmap.dav_store()?;
    let Some(range) = range else {
        return load_events(store.as_ref(), ctx.scope, &[cal]).await;
    };
    match store.in_range(ctx.scope, cal, range).await {
        Ok(mut objs) => {
            objs.retain(|o| o.index.component == Component::Event);
            Ok(objs)
        }
        // Календарь удалили между запросами.
        Err(Error::NotFound) => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// Идентификаторы событий (или вхождений) по фильтру и сортировке.
async fn run_query(ctx: &Ctx<'_>, a: &QueryArgs) -> MethodResult<Vec<String>> {
    let sort = a.sort.as_deref().unwrap_or_default();
    if sort
        .iter()
        .any(|c| !SORT_PROPS.contains(&c.property.as_str()))
    {
        return Err(MethodError::new("unsupportedSort"));
    }
    let cfg = &ctx.jmap.cfg.calendars;
    let request = request_tz(a.time_zone.as_ref())?;
    let bounds_tz = request.unwrap_or(Tz::UTC);
    let filter = a.filter.as_ref().filter(|f| !f.is_null());
    let (after, before) = top_bounds(filter, bounds_tz);
    if a.expand_recurrences {
        let (Some(from), Some(to)) = (after, before) else {
            return Err(MethodError::invalid(
                "expandRecurrences requires after and before in the filter condition",
            ));
        };
        let max = i64::try_from(cfg.max_expanded_query_duration.as_secs()).unwrap_or(i64::MAX);
        if to.saturating_sub(from) > max {
            return Err(MethodError::invalid(
                "the time range is longer than maxExpandedQueryDuration",
            ));
        }
    }
    let at = |t: Option<i64>, skew: i64| {
        t.and_then(|t| OffsetDateTime::from_unix_timestamp(t.saturating_add(skew)).ok())
    };
    let range = (after.is_some() || before.is_some()).then(|| TimeRange {
        start: at(after, -SKEW),
        end: at(before, SKEW),
    });
    let store = ctx.jmap.dav_store()?;
    let mut zones = Zones::new(request);
    let mut items = Vec::new();
    for cal in calendar_ids(store.as_ref(), ctx.scope).await? {
        let tz = zones.of(store.as_ref(), ctx.scope, cal).await?;
        for obj in candidates(ctx, cal, range).await? {
            let json = event_json(&obj);
            let all = instances(&obj.body, tz, cfg.max_instances);
            let id = obj.id.to_string();
            let found = if a.expand_recurrences {
                all.iter()
                    .filter(|i| overlaps(i, after, before))
                    .filter_map(|i| {
                        let (id, json) = match i.rid {
                            Some(rid) => {
                                let iid = instance_id(&id, rid)?;
                                let json = instance_json(&json, &local_key(rid)?, &iid);
                                (iid, json)
                            }
                            None => (id.clone(), json.clone()),
                        };
                        Some(Item {
                            id,
                            json,
                            instances: vec![*i],
                        })
                    })
                    .collect()
            } else {
                vec![Item {
                    id,
                    json,
                    instances: all,
                }]
            };
            for item in found {
                if match filter {
                    Some(f) => matches(f, &item, bounds_tz)?,
                    None => true,
                } {
                    items.push(item);
                }
            }
        }
    }
    if items.is_empty()
        && let Some(f) = filter
    {
        // Фильтр проверяется и без событий.
        let empty = Item {
            id: String::new(),
            json: Map::new(),
            instances: Vec::new(),
        };
        matches(f, &empty, bounds_tz)?;
    }
    items.sort_by(|x, y| {
        sort.iter()
            .map(|c| {
                let o = match c.property.as_str() {
                    "start" => x.start().cmp(&y.start()),
                    p => x.text(p).cmp(y.text(p)),
                };
                if c.is_ascending { o } else { o.reverse() }
            })
            .find(|o| *o != Ordering::Equal)
            .unwrap_or_else(|| x.id.cmp(&y.id))
    });
    Ok(items.into_iter().map(|i| i.id).collect())
}

/// `CalendarEvent/query`.
pub(crate) async fn query(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: QueryArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let st = state(ctx.jmap.dav_store()?.as_ref(), ctx.scope).await?;
    let ids = run_query(ctx, &a).await?;
    let mut out = object(json!({
        "accountId": a.account_id,
        "queryState": st,
        "canCalculateChanges": !a.expand_recurrences,
    }));
    apply_window(&ids, &a.window, ctx.jmap.cfg.max_query_limit, &mut out)?;
    Ok(out)
}

/// `CalendarEvent/queryChanges` по журналу объектов календарей; с `expandRecurrences` —
/// `cannotCalculateChanges`.
pub(crate) async fn query_changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let expand = args.get("expandRecurrences") == Some(&Value::Bool(true));
    let mut q = args.clone();
    q.retain(|k, _| matches!(k.as_str(), "accountId" | "filter" | "sort" | "timeZone"));
    let a: QueryArgs = parse_args(q)?;
    let mut rest = args;
    rest.retain(|k, _| !matches!(k.as_str(), "expandRecurrences" | "timeZone"));
    let qc: QueryChangesArgs = parse_args(rest)?;
    ctx.check_account(&qc.account_id)?;
    if expand {
        return Err(MethodError::new("cannotCalculateChanges"));
    }
    let limit = qc.limit(ctx.jmap.cfg.max_changes)?;
    let since = parse_state(&qc.since_query_state)?;
    let c = ctx
        .jmap
        .dav_store()?
        .account_changes(ctx.scope, KIND, since, limit)
        .await
        .map_err(changes_error)?;
    let c = map_changes(c, ToString::to_string);
    let ids = run_query(ctx, &a).await?;
    query_changes_response(&qc, &ids, &c, limit)
}

/// Пояс по имени (для тестов границ).
#[cfg(test)]
fn zone(name: &str) -> Tz {
    Tz::from_str(name).unwrap_or(Tz::UTC)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{bound, zone};
    use crate::calendar_event::tests::{by_uid, put_dataset};
    use crate::calendars::tests::{call, create_calendar};
    use crate::testenv::Env;

    /// `uid` результатов запроса (вхождения — `uid@recurrenceId`), по порядку ответа.
    async fn uids(env: &Env, args: Value) -> Vec<String> {
        let mut a = args;
        a["accountId"] = json!(env.account());
        let r = call(env, "CalendarEvent/query", a).await;
        let ids = r["ids"].as_array().unwrap_or_else(|| panic!("{r}")).clone();
        let g = call(
            env,
            "CalendarEvent/get",
            json!({"accountId": env.account(), "ids": ids, "properties": ["uid", "recurrenceId"]}),
        )
        .await;
        assert_eq!(g["notFound"], json!([]), "{g}");
        g["list"]
            .as_array()
            .expect("list")
            .iter()
            .map(|e| match e["recurrenceId"].as_str() {
                Some(r) => format!("{}@{r}", e["uid"].as_str().expect("uid")),
                None => e["uid"].as_str().expect("uid").to_owned(),
            })
            .collect()
    }

    async fn in_range(env: &Env, after: &str, before: &str) -> Vec<String> {
        let mut out = uids(env, json!({"filter": {"after": after, "before": before}})).await;
        out.sort();
        out
    }

    #[test]
    fn bounds() {
        assert_eq!(
            bound(&json!("2026-01-12T07:00:00Z"), zone("UTC")),
            Some(1_768_201_200)
        );
        assert_eq!(
            bound(&json!("2026-01-12T10:00:00"), zone("Europe/Moscow")),
            Some(1_768_201_200)
        );
        assert_eq!(bound(&json!("2026-01-12"), zone("UTC")), None);
        assert_eq!(bound(&json!(5), zone("UTC")), None);
    }

    /// Те же ожидания, что у `calendar-query` DAV-06 (`time_range_recurrence`).
    #[tokio::test]
    async fn time_range_recurrence() {
        let env = Env::new().await;
        put_dataset(&env).await;
        let cases = [
            ("2026-01-05T00:00:00Z", "2026-01-06T00:00:00Z", vec!["ev-1"]),
            (
                "2026-01-06T09:30:00Z",
                "2026-01-06T09:40:00Z",
                vec!["weekly"],
            ),
            // Конец вхождения — не в интервале, начало интервала — граница.
            ("2026-01-06T10:00:00Z", "2026-01-06T12:00:00Z", vec![]),
            // 13-е исключено EXDATE.
            ("2026-01-13T00:00:00Z", "2026-01-14T00:00:00Z", vec![]),
            // 20-е перенесено RECURRENCE-ID на 21-е.
            ("2026-01-20T00:00:00Z", "2026-01-21T00:00:00Z", vec![]),
            (
                "2026-01-21T12:00:00Z",
                "2026-01-21T18:00:00Z",
                vec!["weekly"],
            ),
            (
                "2026-01-27T00:00:00Z",
                "2026-01-28T00:00:00Z",
                vec!["weekly"],
            ),
            // COUNT=4: дальше вхождений нет.
            ("2026-02-03T00:00:00Z", "2026-03-01T00:00:00Z", vec![]),
            (
                "2026-01-01T00:00:00Z",
                "2026-02-01T00:00:00Z",
                vec!["allday", "ev-1", "moscow", "weekly"],
            ),
        ];
        for (after, before, want) in cases {
            assert_eq!(
                in_range(&env, after, before).await,
                want,
                "{after}–{before}"
            );
        }
        // Открытые границы.
        assert_eq!(
            uids(&env, json!({"filter": {"after": "2026-01-27T00:00:00Z"}})).await,
            ["weekly"]
        );
        assert_eq!(
            uids(&env, json!({"filter": {"before": "2026-01-05T11:00:00Z"}})).await,
            ["ev-1"]
        );
    }

    /// Те же ожидания, что у `calendar-query` DAV-06 (`time_range_all_day_and_time_zones`).
    #[tokio::test]
    async fn time_range_all_day_and_time_zones() {
        let env = Env::new().await;
        let acc = env.account();
        let (_, cal, _) = put_dataset(&env).await;
        // Событие в поясе Europe/Moscow: 10:00 MSK = 07:00Z.
        assert_eq!(
            in_range(&env, "2026-01-12T06:30:00Z", "2026-01-12T07:30:00Z").await,
            ["moscow"]
        );
        assert!(
            in_range(&env, "2026-01-12T09:00:00Z", "2026-01-12T12:00:00Z")
                .await
                .is_empty()
        );
        // «Весь день» 10 января: в календаре без пояса — сутки UTC.
        let late = ("2026-01-10T21:30:00Z", "2026-01-10T22:00:00Z");
        assert_eq!(in_range(&env, late.0, late.1).await, ["allday"]);
        // В поясе календаря Europe/Moscow тот же день — 9 января 21:00Z … 10 января 21:00Z.
        let r = call(
            &env,
            "Calendar/set",
            json!({"accountId": acc, "update": {&cal: {"timeZone": "Europe/Moscow"}}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        assert!(in_range(&env, late.0, late.1).await.is_empty());
        assert_eq!(
            in_range(&env, "2026-01-09T21:30:00Z", "2026-01-09T22:00:00Z").await,
            ["allday"]
        );
        // Пояс запроса важнее пояса календаря.
        let filter = json!({"after": late.0, "before": late.1});
        assert_eq!(
            uids(&env, json!({"filter": filter, "timeZone": "Etc/UTC"})).await,
            ["allday"]
        );
        // Границы `LocalDateTime` — в поясе запроса: 10:30 MSK = 07:30Z.
        let filter = json!({"after": "2026-01-12T10:30:00", "before": "2026-01-12T10:40:00"});
        assert_eq!(
            uids(&env, json!({"filter": filter, "timeZone": "Europe/Moscow"})).await,
            ["moscow"]
        );
        assert!(uids(&env, json!({"filter": filter})).await.is_empty());
    }

    #[tokio::test]
    async fn filters_and_sort() {
        let env = Env::new().await;
        let acc = env.account();
        let (_, cal, _) = put_dataset(&env).await;
        let other = create_calendar(&env, "Личное").await;
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc, "create": {"e": {
                "calendarIds": {&other: true}, "uid": "own", "title": "Обед", "description": "с Анной",
                "start": "2026-01-08T12:00:00", "timeZone": "Etc/UTC", "duration": "PT1H",
                "organizerCalendarAddress": "mailto:ivan@example.org",
                "participants": {"p0": {"@type": "Participant", "name": "Иван",
                    "calendarAddress": "mailto:ivan@example.org", "roles": {"owner": true}}}}}}),
        )
        .await;
        assert!(r["notCreated"].is_null(), "{r}");

        // Сортировка по началу первого вхождения.
        let by_start = json!([{"property": "start"}]);
        assert_eq!(
            uids(&env, json!({"sort": by_start})).await,
            ["ev-1", "weekly", "own", "allday", "moscow"]
        );
        assert_eq!(
            uids(
                &env,
                json!({"sort": [{"property": "start", "isAscending": false}], "limit": 2})
            )
            .await,
            ["moscow", "allday"]
        );
        assert_eq!(
            uids(&env, json!({"sort": [{"property": "uid"}], "position": 3})).await,
            ["own", "weekly"]
        );

        let q = |filter: Value| {
            uids(
                &env,
                json!({"filter": filter, "sort": [{"property": "start"}]}),
            )
        };
        assert_eq!(q(json!({"inCalendar": other})).await, ["own"]);
        assert_eq!(q(json!({"inCalendars": [other, cal]})).await.len(), 5);
        assert_eq!(
            q(json!({"inCalendars": [cal], "title": "ВСТРЕЧА"})).await,
            ["moscow"]
        );
        assert_eq!(q(json!({"uid": "allday"})).await, ["allday"]);
        assert_eq!(q(json!({"description": "анной"})).await, ["own"]);
        assert_eq!(q(json!({"location": "переговорная"})).await, ["moscow"]);
        assert_eq!(q(json!({"attendee": "petr@example.org"})).await, ["moscow"]);
        assert_eq!(q(json!({"attendee": "ivan"})).await, Vec::<String>::new());
        assert_eq!(q(json!({"owner": "иван"})).await, ["own"]);
        // `text`: название, описание, место, участники и исключения повтора.
        assert_eq!(q(json!({"text": "пётр"})).await, ["moscow"]);
        assert_eq!(q(json!({"text": "перенос"})).await, ["weekly"]);
        assert_eq!(
            q(json!({"operator": "OR", "conditions": [{"title": "обед"}, {"title": "выходной"}]}))
                .await,
            ["own", "allday"]
        );
        assert_eq!(
            q(json!({"operator": "NOT", "conditions": [{"inCalendar": cal}]})).await,
            ["own"]
        );
        assert_eq!(
            q(json!({"operator": "AND", "conditions": [
                {"after": "2026-01-06T00:00:00Z", "before": "2026-01-09T00:00:00Z"},
                {"operator": "NOT", "conditions": [{"title": "обед"}]}]}))
            .await,
            ["weekly"]
        );

        for (args, error) in [
            (json!({"filter": {"flavour": "x"}}), "unsupportedFilter"),
            (json!({"filter": {"after": "вчера"}}), "unsupportedFilter"),
            (
                json!({"filter": {"operator": "XOR", "conditions": []}}),
                "invalidArguments",
            ),
            (json!({"sort": [{"property": "title"}]}), "unsupportedSort"),
            (json!({"timeZone": "Mars/Olympus"}), "invalidArguments"),
            (json!({"anchor": "nope"}), "anchorNotFound"),
        ] {
            let mut a = args;
            a["accountId"] = json!(acc);
            let r = call(&env, "CalendarEvent/query", a).await;
            assert_eq!(r["type"], error, "{r}");
        }
    }

    /// `expandRecurrences`: вхождения интервала отдельными результатами (как `expand` у `calendar-query`).
    #[tokio::test]
    async fn expand_recurrences() {
        let env = Env::new().await;
        let acc = env.account();
        put_dataset(&env).await;
        let month = json!({"after": "2026-01-01T00:00:00Z", "before": "2026-02-01T00:00:00Z"});
        let args =
            json!({"filter": month, "expandRecurrences": true, "sort": [{"property": "start"}]});
        assert_eq!(
            uids(&env, args.clone()).await,
            [
                "ev-1",
                "weekly@2026-01-06T09:00:00",
                "allday",
                "moscow",
                // Перенос 20-го на 21-е; 13-е исключено.
                "weekly@2026-01-20T09:00:00",
                "weekly@2026-01-27T09:00:00",
            ]
        );
        let mut a = args.clone();
        a["accountId"] = json!(acc);
        a["calculateTotal"] = json!(true);
        let r = call(&env, "CalendarEvent/query", a).await;
        assert_eq!(r["canCalculateChanges"], false, "{r}");
        assert_eq!(r["total"], 6, "{r}");
        // Вхождение читается по `id` из запроса: время и название исключения.
        let weekly = by_uid(&env).await["weekly"]["id"]
            .as_str()
            .expect("id")
            .to_owned();
        assert_eq!(r["ids"][4], format!("{weekly}_20260120T090000"), "{r}");
        let g = call(
            &env,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": [r["ids"][4]], "properties": ["title", "utcStart"]}),
        )
        .await;
        assert_eq!(g["list"][0]["title"], "Перенос", "{g}");
        assert_eq!(g["list"][0]["utcStart"], "2026-01-21T15:00:00Z", "{g}");

        // Только вхождения интервала; фильтр по названию — по вхождению с исключением.
        let week = json!({"after": "2026-01-20T00:00:00Z", "before": "2026-01-28T00:00:00Z"});
        assert_eq!(
            uids(&env, json!({"filter": week, "expandRecurrences": true}))
                .await
                .len(),
            2
        );
        let moved = json!({"after": "2026-01-01T00:00:00Z", "before": "2026-02-01T00:00:00Z",
                           "title": "перенос"});
        assert_eq!(
            uids(&env, json!({"filter": moved, "expandRecurrences": true})).await,
            ["weekly@2026-01-20T09:00:00"]
        );

        // Нужны обе границы и интервал не длиннее лимита.
        for filter in [
            json!({"after": "2026-01-01T00:00:00Z"}),
            json!({"after": "2026-01-01T00:00:00Z", "before": "2028-01-01T00:00:00Z"}),
            json!({"operator": "AND", "conditions": [month]}),
        ] {
            let r = call(
                &env,
                "CalendarEvent/query",
                json!({"accountId": acc, "filter": filter, "expandRecurrences": true}),
            )
            .await;
            assert_eq!(r["type"], "invalidArguments", "{r}");
        }
        let r = call(
            &env,
            "CalendarEvent/queryChanges",
            json!({"accountId": acc, "filter": month, "expandRecurrences": true,
                   "sinceQueryState": r["queryState"]}),
        )
        .await;
        assert_eq!(r["type"], "cannotCalculateChanges", "{r}");
    }

    #[tokio::test]
    async fn query_changes() {
        let env = Env::new().await;
        let acc = env.account();
        put_dataset(&env).await;
        let events = by_uid(&env).await;
        let id = |uid: &str| events[uid]["id"].as_str().expect("id").to_owned();
        let args = json!({"accountId": acc, "sort": [{"property": "start"}],
            "filter": {"after": "2026-01-05T00:00:00Z", "before": "2026-01-11T00:00:00Z"}});
        let q = call(&env, "CalendarEvent/query", args.clone()).await;
        assert_eq!(
            q["ids"],
            json!([id("ev-1"), id("weekly"), id("allday")]),
            "{q}"
        );
        assert_eq!(q["canCalculateChanges"], true);

        // Одно событие уходит из интервала, одно удаляется, одно появляется.
        let r = call(
            &env,
            "CalendarEvent/set",
            json!({"accountId": acc,
                "create": {"n": {"uid": "new", "title": "Новое", "start": "2026-01-05T08:00:00",
                                 "timeZone": "Etc/UTC", "duration": "PT30M"}},
                "update": {id("ev-1"): {"start": "2026-02-05T10:00:00"}},
                "destroy": [id("allday")]}),
        )
        .await;
        let new = r["created"]["n"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{r}"));
        assert!(
            r["notUpdated"].is_null() && r["notDestroyed"].is_null(),
            "{r}"
        );

        let mut a = args.clone();
        a["sinceQueryState"] = q["queryState"].clone();
        a["calculateTotal"] = json!(true);
        let c = call(&env, "CalendarEvent/queryChanges", a.clone()).await;
        let mut removed: Vec<String> = c["removed"]
            .as_array()
            .unwrap_or_else(|| panic!("{c}"))
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        removed.sort();
        let mut want = vec![id("ev-1"), id("allday")];
        want.sort();
        assert_eq!(removed, want, "{c}");
        assert_eq!(c["added"], json!([{"id": new, "index": 0}]), "{c}");
        assert_eq!(c["total"], 2, "{c}");
        let q = call(&env, "CalendarEvent/query", args).await;
        assert_eq!(q["ids"], json!([new, id("weekly")]), "{q}");
        assert_eq!(c["newQueryState"], q["queryState"]);

        a["sinceQueryState"] = json!("nope");
        let c = call(&env, "CalendarEvent/queryChanges", a).await;
        assert_eq!(c["type"], "cannotCalculateChanges", "{c}");
    }
}
