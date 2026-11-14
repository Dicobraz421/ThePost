//! JMAP Tasks (JMAP-33; draft-ietf-jmap-tasks, SRS 4.4 — транспорт синхронизации): `TaskList/get`, `/changes`,
//! `/set` и `Task/get`, `/changes`, `/query`, `/queryChanges`, `/set` поверх `DavStore`.
//!
//! Список задач — коллекция календаря с `VTODO` среди компонентов (`supported-calendar-component-set` CalDAV): та
//! же, что видит CalDAV-клиент (Thunderbird); календарь по умолчанию с `VEVENT` и `VTODO` — и календарь, и список
//! задач. `TaskList/set` создаёт коллекцию только с `VTODO` (в `Calendar/get` её нет). Задача — `VTODO` ресурса
//! (главный компонент; JSCalendar `Task` ↔ iCalendar — calcard), прочие компоненты и свойства `VCALENDAR` при
//! изменении из JMAP сохраняются. Состояние — общее с календарями (`DavStore::state` вида), push — по `DavChanged`.
//!
//! Отличия от черновика:
//! - capability расширений (`assignees`, `alerts`, `multilingual`, `custom-timezones`) не объявляются; `role`
//!   списка — `null`, `keywordColors`, `categoryColors` — `null`, `workflowStatuses` — значения `progress`
//!   JSCalendar;
//! - задача — в одном списке; перенос сменой `taskListId` не поддерживается (как у событий); `utcStart`, `utcDue`
//!   и вхождения повторов не вычисляются;
//! - фильтр `Task/query` — `taskListId`, `uid`, `text` (название и описание), `title`, `description`, `progress`,
//!   операторы `AND`, `OR`, `NOT`; сортировка — `start`, `due`, `created`, `updated`, `title`, `uid`, `progress` по
//!   строковому значению;
//! - в аккаунте владельца общих календарей (JMAP-32) видны списки с правом чтения, задачи меняются с правом записи,
//!   список не создаётся.

use std::cmp::Ordering;
use std::str::FromStr;

use calcard::common::timezone::Tz;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tp_contracts::Error;
use tp_contracts::dav::{
    Collection, CollectionId, Component, DavObject, DavObjectId, DeleteOutcome, Precondition,
    PutOutcome, default_of,
};

use crate::calendar_event::{entry_ical, index_options, invalid, split};
use crate::calendars::{KIND, state};
use crate::contact_card::{apply_patch, contains, load};
use crate::contacts::store_error;
use crate::core::Ctx;
use crate::core::error::{MethodError, MethodResult, changes_error, set_error};
use crate::core::request::{
    ChangesArgs, Comparator, GetArgs, QueryChangesArgs, QueryWindow, SetArgs, SetOutcome,
    apply_window, changes_response, map_changes, object, parse_args, parse_state,
    query_changes_response, select,
};
use crate::email_get::utc_date;
use crate::sharing::{
    ObjectType, commit_share, prepare_share, share_with, split_share, task_list_rights,
};

/// Сегмент URL нового списка — его `id`, отображаемое имя по умолчанию.
const DEFAULT_NAME: &str = tp_contracts::dav::DEFAULT_COLLECTION_NAME;

/// Значения `progress` JSCalendar (`workflowStatuses` списка).
const STATUSES: [&str; 6] = [
    "needs-action",
    "in-process",
    "completed",
    "failed",
    "cancelled",
    "pending",
];

/// Значение capability аккаунта; `may_create` — свой аккаунт (в чужом списки не создаются).
pub(crate) fn account_capability(may_create: bool) -> Value {
    json!({
        "minDateTime": "0001-01-01T00:00:00",
        "maxDateTime": "9999-12-31T23:59:59",
        "mayCreateTaskList": may_create,
    })
}

/// Коллекция — список задач (`VTODO` среди компонентов).
pub(crate) fn is_task_list(c: &Collection) -> bool {
    c.components.contains(&Component::Todo)
}

/// Списки задач аккаунта вызова (видимые).
async fn task_lists(ctx: &Ctx<'_>) -> MethodResult<Vec<Collection>> {
    let mut all = ctx
        .jmap
        .dav_store()?
        .list_collections(ctx.scope, KIND)
        .await?;
    all.retain(|c| is_task_list(c) && ctx.access.sees(c.id));
    Ok(all)
}

/// Список задач по умолчанию среди списков `lists` — правило [`default_of`].
fn default_list(lists: &[Collection]) -> Option<CollectionId> {
    default_of(lists).map(|c| c.id)
}

// ---------- TaskList ----------

async fn list_object(
    ctx: &Ctx<'_>,
    c: &Collection,
    default: Option<CollectionId>,
) -> MethodResult<Map<String, Value>> {
    Ok(object(json!({
        "id": c.id.to_string(),
        "role": null,
        "name": c.display_name,
        "description": c.description,
        "color": c.color,
        "keywordColors": null,
        "categoryColors": null,
        "sortOrder": c.order,
        "isSubscribed": c.subscribed,
        "isDefault": default == Some(c.id),
        "timeZone": c.timezone,
        "workflowStatuses": STATUSES,
        "shareWith": share_with(ctx, ObjectType::TaskList, c).await?,
        "myRights": task_list_rights(ctx.access.rights(c.id), ctx.jmap.sharing.is_some()),
    })))
}

/// `TaskList/get`.
pub(crate) async fn list_get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.open_dav(&a.account_id).await?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let store = ctx.jmap.dav_store()?;
    let st = state(store.as_ref(), ctx.scope).await?;
    let lists = task_lists(ctx).await?;
    let default = default_list(&lists);
    let props = a.properties.as_deref();
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    let wanted: Vec<String> = match &a.ids {
        Some(ids) => ids.iter().map(|i| ctx.resolve_id(i).to_owned()).collect(),
        None => lists.iter().map(|c| c.id.to_string()).collect(),
    };
    let mut seen: Vec<&str> = Vec::new();
    for id in &wanted {
        if seen.contains(&id.as_str()) {
            continue;
        }
        seen.push(id);
        match lists.iter().find(|c| c.id.to_string() == *id) {
            Some(c) => list.push(select(list_object(ctx, c, default).await?, props)),
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

/// `TaskList/changes` по журналу коллекций календаря: только списки задач.
pub(crate) async fn list_changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: ChangesArgs = parse_args(args)?;
    ctx.open_dav(&a.account_id).await?;
    let limit = a.limit(ctx.jmap.cfg.max_changes)?;
    let since = parse_state(&a.since_state)?;
    let store = ctx.jmap.dav_store()?;
    let c = store
        .collection_changes(ctx.scope, KIND, since, limit)
        .await
        .map_err(changes_error)?;
    let mut c = map_changes(c, ToString::to_string);
    crate::sharing::retain_collections(ctx, store.as_ref(), &mut c, is_task_list).await?;
    let mut out = changes_response(&a.account_id, &a.since_state, &c);
    out.insert("updatedProperties".into(), Value::Null);
    Ok(out)
}

/// Применяет свойства `/set` к списку; ошибка — `invalidProperties`.
fn apply_list_props(c: &mut Collection, p: &Map<String, Value>, create: bool) -> Result<(), Value> {
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
            ("color", Value::Null) => {
                c.color = None;
                true
            }
            ("color", Value::String(s)) => {
                c.color = Some(s.clone());
                true
            }
            ("sortOrder", Value::Number(n)) => n
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .map(|n| c.order = n)
                .is_some(),
            ("isSubscribed", Value::Bool(b)) => {
                c.subscribed = *b;
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
            ("role" | "keywordColors" | "categoryColors", Value::Null) => true,
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

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListSetExtra {
    #[serde(default)]
    on_destroy_remove_tasks: bool,
}

/// Список задач `id` аккаунта вызова; нет или не виден — `None`.
async fn find_list(ctx: &Ctx<'_>, id: &str) -> MethodResult<Option<Collection>> {
    let Ok(cid) = CollectionId::from_str(id) else {
        return Ok(None);
    };
    let c = ctx.jmap.dav_store()?.get_collection(ctx.scope, cid).await?;
    Ok(c.filter(|c| c.kind == KIND && is_task_list(c) && ctx.access.sees(c.id)))
}

/// `TaskList/set`: создание (коллекция только с `VTODO`), изменение (имя, описание, цвет, порядок, подписка, пояс,
/// `shareWith`), удаление (`onDestroyRemoveTasks`). Коллекция и с `VEVENT` удаляется только `Calendar/set`.
pub(crate) async fn list_set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let mut rest = args.clone();
    rest.retain(|k, _| k.starts_with("on"));
    let extra: ListSetExtra = parse_args(rest)?;
    let mut base = args;
    base.retain(|k, _| !k.starts_with("on"));
    let a: SetArgs = parse_args(base)?;
    ctx.open_dav(&a.account_id).await?;
    a.check_limit(ctx.jmap.cfg.max_objects_in_set)?;
    let store = ctx.jmap.dav_store()?.clone();
    let old = state(store.as_ref(), ctx.scope).await?;
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }
    let ty = ObjectType::TaskList;
    let mut out = SetOutcome::default();
    for (cid, v) in a.create.unwrap_or_default() {
        if !ctx.access.is_own() {
            out.not_created
                .insert(cid, set_error("forbidden", None, &[]));
            continue;
        }
        let (props, share) = split_share(&v);
        let mut c = Collection::new(KIND, DEFAULT_NAME);
        c.name = c.id.to_string();
        c.components = [Component::Todo].into();
        if let Err(e) = apply_list_props(&mut c, &props, true) {
            out.not_created.insert(cid, e);
            continue;
        }
        let grants = match prepare_share(ctx, ty, None, &share).await? {
            Ok(g) => g,
            Err(e) => {
                out.not_created.insert(cid, e);
                continue;
            }
        };
        match store.create_collection(ctx.scope, c.clone()).await {
            Ok(_) => {
                commit_share(ctx, ty, &c, grants).await?;
                ctx.created.insert(cid.clone(), c.id.to_string());
                let lists = task_lists(ctx).await?;
                out.created.insert(
                    cid,
                    json!({
                        "id": c.id.to_string(),
                        "role": null,
                        "isDefault": default_list(&lists) == Some(c.id),
                        "isSubscribed": c.subscribed,
                        "workflowStatuses": STATUSES,
                        "shareWith": share_with(ctx, ty, &c).await?,
                        "myRights": task_list_rights(ctx.access.rights(c.id), ctx.jmap.sharing.is_some()),
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
        let Some(mut c) = find_list(ctx, &rid).await? else {
            out.not_updated.insert(id, set_error("notFound", None, &[]));
            continue;
        };
        if !ctx.access.admins(c.id) {
            out.not_updated
                .insert(id, set_error("forbidden", None, &[]));
            continue;
        }
        let (props, share) = split_share(&patch);
        if let Err(e) = apply_list_props(&mut c, &props, false) {
            out.not_updated.insert(id, e);
            continue;
        }
        let grants = match prepare_share(ctx, ty, Some(c.id), &share).await? {
            Ok(g) => g,
            Err(e) => {
                out.not_updated.insert(id, e);
                continue;
            }
        };
        if !props.is_empty()
            && let Err(e) = store.update_collection(ctx.scope, c.clone()).await
        {
            out.not_updated.insert(id, store_error(e)?);
            continue;
        }
        commit_share(ctx, ty, &c, grants).await?;
        out.updated.insert(id, Value::Null);
    }
    for id in a.destroy.unwrap_or_default() {
        let rid = ctx.resolve_id(&id).to_owned();
        let Some(c) = find_list(ctx, &rid).await? else {
            out.not_destroyed
                .insert(id, set_error("notFound", None, &[]));
            continue;
        };
        if !ctx.access.admins(c.id) {
            out.not_destroyed
                .insert(id, set_error("forbidden", None, &[]));
            continue;
        }
        if crate::calendars::is_calendar(&c) {
            let why = "the task list is also a calendar: destroy it with Calendar/set";
            out.not_destroyed
                .insert(id, set_error("forbidden", Some(why), &[]));
            continue;
        }
        if !extra.on_destroy_remove_tasks && !store.list_objects(ctx.scope, c.id).await?.is_empty()
        {
            out.not_destroyed
                .insert(id, set_error("taskListHasTask", None, &[]));
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

// ---------- Task ----------

/// Свойства JMAP задачи, которых нет в JSCalendar.
const JMAP_PROPS: [&str; 4] = ["id", "taskListId", "utcStart", "utcDue"];

/// Задача JMAP из объекта: JSCalendar `Task` из `VTODO` и свойства JMAP (`id`, `taskListId`). Неразбираемое тело —
/// задача только с `uid` из индекса.
fn task_json(obj: &DavObject) -> Map<String, Value> {
    let mut m = split(&obj.body, "Task").map(|(_, t)| t).unwrap_or_default();
    m.entry("@type").or_insert_with(|| json!("Task"));
    m.entry("uid").or_insert_with(|| json!(obj.index.uid));
    m.insert("id".into(), json!(obj.id.to_string()));
    m.insert("taskListId".into(), json!(obj.collection.to_string()));
    m
}

/// Задача по `id`; нет объекта, не задача или список без права чтения — `None`.
async fn find(ctx: &Ctx<'_>, id: &str) -> MethodResult<Option<DavObject>> {
    let Ok(oid) = DavObjectId::from_str(id) else {
        return Ok(None);
    };
    let store = ctx.jmap.dav_store()?;
    let obj = store
        .objects_by_id(ctx.scope, &[oid])
        .await?
        .into_iter()
        .next();
    Ok(obj.filter(|o| o.index.component == Component::Todo && ctx.access.reads(o.collection)))
}

/// Задачи видимых списков с правом чтения.
async fn load_tasks(ctx: &Ctx<'_>) -> MethodResult<Vec<DavObject>> {
    let lists: Vec<CollectionId> = task_lists(ctx)
        .await?
        .into_iter()
        .map(|c| c.id)
        .filter(|c| ctx.access.reads(*c))
        .collect();
    let store = ctx.jmap.dav_store()?;
    let mut all = load(store.as_ref(), ctx.scope, &lists).await?;
    all.retain(|o| o.index.component == Component::Todo);
    Ok(all)
}

/// `Task/get`.
pub(crate) async fn get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.open_dav(&a.account_id).await?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let st = state(ctx.jmap.dav_store()?.as_ref(), ctx.scope).await?;
    let props = a.properties.as_deref();
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    match &a.ids {
        None => list.extend(
            load_tasks(ctx)
                .await?
                .iter()
                .map(|o| select(task_json(o), props)),
        ),
        Some(ids) => {
            let mut seen: Vec<&str> = Vec::new();
            for id in ids {
                let id = ctx.resolve_id(id);
                if seen.contains(&id) {
                    continue;
                }
                seen.push(id);
                match find(ctx, id).await? {
                    Some(o) => list.push(select(task_json(&o), props)),
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

/// Изменения задач после `since`: созданные и изменённые — только задачи списков с правом чтения.
async fn task_changes(
    ctx: &Ctx<'_>,
    since: &str,
    limit: usize,
) -> MethodResult<tp_contracts::meta::Changes<String>> {
    let since = parse_state(since)?;
    let store = ctx.jmap.dav_store()?;
    let c = store
        .account_changes(ctx.scope, KIND, since, limit)
        .await
        .map_err(changes_error)?;
    let mut c = map_changes(c, ToString::to_string);
    crate::sharing::retain_objects(ctx, store.as_ref(), &mut c, |o| {
        o.index.component == Component::Todo && ctx.access.reads(o.collection)
    })
    .await?;
    Ok(c)
}

/// `Task/changes` по журналу объектов календарей (удалённые — все удалённые объекты журнала).
pub(crate) async fn changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: ChangesArgs = parse_args(args)?;
    ctx.open_dav(&a.account_id).await?;
    let limit = a.limit(ctx.jmap.cfg.max_changes)?;
    let c = task_changes(ctx, &a.since_state, limit).await?;
    Ok(changes_response(&a.account_id, &a.since_state, &c))
}

// ---------- Запрос ----------

/// Свойства сортировки.
const SORT_PROPS: [&str; 7] = [
    "start", "due", "created", "updated", "title", "uid", "progress",
];

/// Строковое значение свойства задачи (сортировка, сравнение).
fn text<'a>(task: &'a Map<String, Value>, prop: &str) -> &'a str {
    task.get(prop).and_then(Value::as_str).unwrap_or_default()
}

/// Условие фильтра `Task/query`; неизвестное — `unsupportedFilter`.
fn matches(filter: &Value, task: &Map<String, Value>) -> MethodResult<bool> {
    let unsupported = || MethodError::new("unsupportedFilter");
    let f = filter.as_object().ok_or_else(unsupported)?;
    if let Some(op) = f.get("operator") {
        let conds = f
            .get("conditions")
            .and_then(Value::as_array)
            .ok_or_else(unsupported)?;
        let mut results = Vec::with_capacity(conds.len());
        for c in conds {
            results.push(matches(c, task)?);
        }
        return match op.as_str() {
            Some("AND") => Ok(results.iter().all(|r| *r)),
            Some("OR") => Ok(results.iter().any(|r| *r)),
            Some("NOT") => Ok(!results.iter().any(|r| *r)),
            _ => Err(unsupported()),
        };
    }
    let mut ok = true;
    for (key, val) in f {
        let s = val.as_str().ok_or_else(unsupported)?;
        let needle = s.to_lowercase();
        ok &= match key.as_str() {
            "taskListId" => text(task, "taskListId") == s,
            "uid" => text(task, "uid") == s,
            "progress" => {
                let p = task.get("progress").and_then(Value::as_str);
                p.unwrap_or("needs-action") == s
            }
            "title" => contains(task, &["title"], &needle),
            "description" => contains(task, &["description"], &needle),
            "text" => contains(task, &["title", "description"], &needle),
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

/// Идентификаторы задач по фильтру и сортировке.
async fn run_query(ctx: &Ctx<'_>, a: &QueryArgs) -> MethodResult<Vec<String>> {
    let sort = a.sort.as_deref().unwrap_or_default();
    if sort
        .iter()
        .any(|c| !SORT_PROPS.contains(&c.property.as_str()))
    {
        return Err(MethodError::new("unsupportedSort"));
    }
    let filter = a.filter.as_ref().filter(|f| !f.is_null());
    if let Some(f) = filter {
        // Фильтр проверяется и без задач.
        matches(f, &Map::new())?;
    }
    let mut tasks = Vec::new();
    for obj in load_tasks(ctx).await? {
        let t = task_json(&obj);
        if filter.map_or(Ok(true), |f| matches(f, &t))? {
            tasks.push(t);
        }
    }
    tasks.sort_by(|x, y| {
        sort.iter()
            .map(|c| {
                let o = text(x, &c.property).cmp(text(y, &c.property));
                if c.is_ascending { o } else { o.reverse() }
            })
            .find(|o| *o != Ordering::Equal)
            .unwrap_or_else(|| text(x, "id").cmp(text(y, "id")))
    });
    Ok(tasks.iter().map(|t| text(t, "id").to_owned()).collect())
}

/// `Task/query`.
pub(crate) async fn query(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: QueryArgs = parse_args(args)?;
    ctx.open_dav(&a.account_id).await?;
    let st = state(ctx.jmap.dav_store()?.as_ref(), ctx.scope).await?;
    let ids = run_query(ctx, &a).await?;
    let mut out = object(json!({
        "accountId": a.account_id,
        "queryState": st,
        "canCalculateChanges": true,
    }));
    apply_window(&ids, &a.window, ctx.jmap.cfg.max_query_limit, &mut out)?;
    Ok(out)
}

/// `Task/queryChanges` по журналу объектов календарей.
pub(crate) async fn query_changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let qc: QueryChangesArgs = parse_args(args.clone())?;
    ctx.open_dav(&qc.account_id).await?;
    let mut q = args;
    q.retain(|k, _| matches!(k.as_str(), "accountId" | "filter" | "sort"));
    let a: QueryArgs = parse_args(q)?;
    let limit = qc.limit(ctx.jmap.cfg.max_changes)?;
    let c = task_changes(ctx, &qc.since_query_state, limit).await?;
    let ids = run_query(ctx, &a).await?;
    query_changes_response(&qc, &ids, &c, limit)
}

// ---------- Изменение ----------

/// Итог записи как ошибка `/set`; `Ok` — идентификатор объекта.
fn put_error(r: tp_contracts::Result<PutOutcome>) -> MethodResult<Result<DavObjectId, Value>> {
    Ok(match r {
        Ok(PutOutcome::Stored(p)) => Ok(p.id),
        Ok(PutOutcome::UidConflict { .. }) => Err(invalid(
            "uid is used by another task of the task list",
            &["uid"],
        )),
        Ok(PutOutcome::PreconditionFailed) => Err(set_error(
            "serverFail",
            Some("the task was changed concurrently"),
            &[],
        )),
        Err(Error::NotFound) => Err(invalid("task list not found", &["taskListId"])),
        Err(e) => Err(store_error(e)?),
    })
}

/// iCalendar и индекс задачи (свойства JMAP отбрасываются); ошибка — `invalidProperties`.
fn to_ical(
    ctx: &Ctx<'_>,
    group: Map<String, Value>,
    mut task: Map<String, Value>,
) -> Result<(String, tp_contracts::dav::ObjectIndex), Value> {
    for p in JMAP_PROPS {
        task.remove(p);
    }
    entry_ical(group, task, &index_options(ctx))
}

fn forbidden() -> Value {
    set_error("forbidden", Some("mayWriteAll is required"), &[])
}

/// Свойства, которые клиент не задаёт.
fn server_props(task: &Map<String, Value>) -> Option<Value> {
    let bad: Vec<&str> = ["utcStart", "utcDue"]
        .into_iter()
        .filter(|p| task.contains_key(*p))
        .collect();
    (!bad.is_empty()).then(|| set_error("invalidProperties", None, &bad))
}

async fn create_one(ctx: &Ctx<'_>, v: &Value) -> MethodResult<Result<Value, Value>> {
    let store = ctx.jmap.dav_store()?;
    let Value::Object(src) = v else {
        return Ok(Err(set_error("invalidProperties", None, &[])));
    };
    let mut task = src.clone();
    if task.contains_key("id") {
        return Ok(Err(set_error("invalidProperties", None, &["id"])));
    }
    if let Some(e) = server_props(&task) {
        return Ok(Err(e));
    }
    let lists = task_lists(ctx).await?;
    let list = match task.get("taskListId") {
        Some(Value::String(id)) => CollectionId::from_str(ctx.resolve_id(id))
            .ok()
            .filter(|id| lists.iter().any(|l| l.id == *id)),
        Some(_) => None,
        None => default_list(&lists),
    };
    let Some(list) = list else {
        return Ok(Err(invalid(
            "an existing task list is required",
            &["taskListId"],
        )));
    };
    if !ctx.access.writes(list) {
        return Ok(Err(forbidden()));
    }
    let now = utc_date(OffsetDateTime::now_utc());
    let object_id = DavObjectId::new();
    task.insert("@type".into(), json!("Task"));
    task.entry("uid")
        .or_insert_with(|| json!(object_id.to_string()));
    task.entry("created").or_insert_with(|| now.clone());
    task.insert("updated".into(), now.clone());
    let server_set = json!({ "uid": task["uid"], "created": task["created"], "updated": now });
    let group = object(json!({ "@type": "Group", "prodId": "-//The Post//JMAP//RU" }));
    let (body, index) = match to_ical(ctx, group, task) {
        Ok(x) => x,
        Err(e) => return Ok(Err(e)),
    };
    let name = format!("{object_id}.ics");
    let put = store
        .put_object(
            ctx.scope,
            list,
            &name,
            body,
            index,
            Precondition::IfNoneMatch,
        )
        .await;
    Ok(put_error(put)?.map(|id| {
        let mut created = server_set;
        created["id"] = json!(id.to_string());
        for key in ["uid", "created"] {
            if src.contains_key(key) {
                created.as_object_mut().map(|m| m.remove(key));
            }
        }
        created
    }))
}

async fn update_one(ctx: &Ctx<'_>, id: &str, patch: &Value) -> MethodResult<Result<Value, Value>> {
    let store = ctx.jmap.dav_store()?;
    let Some(obj) = find(ctx, id).await? else {
        return Ok(Err(set_error("notFound", None, &[])));
    };
    if !ctx.access.writes(obj.collection) {
        return Ok(Err(forbidden()));
    }
    let Value::Object(patch) = patch else {
        return Ok(Err(set_error("invalidPatch", None, &[])));
    };
    let (group, _) = split(&obj.body, "Task").unwrap_or_default();
    let before = task_json(&obj);
    let mut task = before.clone();
    if let Err(e) = apply_patch(&mut task, patch) {
        return Ok(Err(e));
    }
    for (prop, why) in [
        ("id", "id cannot be changed"),
        ("uid", "uid cannot be changed"),
        ("@type", "@type cannot be changed"),
        (
            "taskListId",
            "moving a task to another task list is not supported",
        ),
    ] {
        if task.get(prop) != before.get(prop) {
            return Ok(Err(invalid(why, &[prop])));
        }
    }
    if let Some(e) = server_props(&task) {
        return Ok(Err(e));
    }
    let now = utc_date(OffsetDateTime::now_utc());
    task.insert("updated".into(), now.clone());
    let (body, index) = match to_ical(ctx, group, task) {
        Ok(x) => x,
        Err(e) => return Ok(Err(e)),
    };
    let pre = Precondition::IfMatch(obj.etag.clone());
    let put = store
        .put_object(ctx.scope, obj.collection, &obj.name, body, index, pre)
        .await;
    Ok(put_error(put)?.map(|_| {
        if patch.contains_key("updated") {
            json!({ "updated": now })
        } else {
            Value::Null
        }
    }))
}

/// `Task/set`.
pub(crate) async fn set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: SetArgs = parse_args(args)?;
    ctx.open_dav(&a.account_id).await?;
    a.check_limit(ctx.jmap.cfg.max_objects_in_set)?;
    let store = ctx.jmap.dav_store()?.clone();
    let old = state(store.as_ref(), ctx.scope).await?;
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }
    let mut out = SetOutcome::default();
    for (cid, v) in a.create.unwrap_or_default() {
        match create_one(ctx, &v).await? {
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
        match update_one(ctx, &rid, &patch).await? {
            Ok(v) => out.updated.insert(id, v),
            Err(e) => out.not_updated.insert(id, e),
        };
    }
    for id in a.destroy.unwrap_or_default() {
        let rid = ctx.resolve_id(&id).to_owned();
        let not_found = || set_error("notFound", None, &[]);
        let Some(obj) = find(ctx, &rid).await? else {
            out.not_destroyed.insert(id, not_found());
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
            Ok(DeleteOutcome::Deleted(_)) => out.destroyed.push(id),
            Ok(DeleteOutcome::PreconditionFailed) | Err(Error::NotFound) => {
                out.not_destroyed.insert(id, not_found());
            }
            Err(e) => {
                out.not_destroyed.insert(id, store_error(e)?);
            }
        }
    }
    let new = state(store.as_ref(), ctx.scope).await?;
    Ok(out.response(&a.account_id, &old, &new))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tp_contracts::dav::DavStore;

    use crate::calendars::tests::{calendar_path, create_calendar, dav_request};
    use crate::contact_card::tests::dav;
    use crate::testenv::Env;

    const USING: &[&str] = &[
        "urn:ietf:params:jmap:core",
        "urn:ietf:params:jmap:calendars",
        "urn:ietf:params:jmap:tasks",
    ];

    async fn call(env: &Env, method: &str, args: Value) -> Value {
        let resp = env
            .request(json!({"using": USING, "methodCalls": [[method, args, "c0"]]}))
            .await;
        let name = &resp["methodResponses"][0][0];
        assert!(name == method || name == "error", "{resp}");
        resp["methodResponses"][0][1].clone()
    }

    /// Сессия: capability задач своего аккаунта.
    #[tokio::test]
    async fn session_lists_capability() {
        let env = Env::new().await;
        let s = env.jmap.session(&env.principal, "").await;
        let acc = env.account();
        assert!(s["capabilities"][USING[2]].is_object(), "{s}");
        let cap = &s["accounts"][&acc]["accountCapabilities"][USING[2]];
        assert_eq!(cap["mayCreateTaskList"], true, "{s}");
        assert_eq!(s["primaryAccounts"][USING[2]], acc);
        let bare = Env::bare().await;
        let s = bare.jmap.session(&bare.principal, "").await;
        assert!(s["capabilities"][USING[2]].is_null(), "{s}");
    }

    /// Списки задач: календарь с `VTODO` — и список задач; список только задач в `Calendar/get` не виден.
    #[tokio::test]
    async fn task_lists() {
        let env = Env::new().await;
        let acc = env.account();
        let cal = create_calendar(&env, "Работа").await;
        let r = call(&env, "TaskList/get", json!({"accountId": acc})).await;
        assert_eq!(r["list"].as_array().map(Vec::len), Some(1), "{r}");
        assert_eq!(r["list"][0]["id"], cal, "{r}");
        assert_eq!(r["list"][0]["myRights"]["mayWriteAll"], true, "{r}");
        let st = r["state"].as_str().expect("state").to_owned();

        let r = call(
            &env,
            "TaskList/set",
            json!({"accountId": acc, "create": {
                "l": {"name": "Дела", "color": "#00ff00"},
                "bad": {"name": "", "role": "inbox"}}}),
        )
        .await;
        let list = r["created"]["l"]["id"].as_str().expect("id").to_owned();
        assert_eq!(r["created"]["l"]["isDefault"], false, "{r}");
        assert_eq!(r["notCreated"]["bad"]["type"], "invalidProperties", "{r}");
        let g = call(&env, "Calendar/get", json!({"accountId": acc})).await;
        assert_eq!(g["list"].as_array().map(Vec::len), Some(1), "{g}");
        let c = call(
            &env,
            "TaskList/changes",
            json!({"accountId": acc, "sinceState": st}),
        )
        .await;
        assert_eq!(c["created"], json!([list]), "{c}");
        let c = call(
            &env,
            "Calendar/changes",
            json!({"accountId": acc, "sinceState": st}),
        )
        .await;
        assert_eq!(c["created"], json!([]), "{c}");

        let r = call(
            &env,
            "TaskList/set",
            json!({"accountId": acc, "update": {&list: {"name": "Личные дела", "sortOrder": 3}}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        let r = call(
            &env,
            "TaskList/get",
            json!({"accountId": acc, "ids": [&list], "properties": ["name", "sortOrder"]}),
        )
        .await;
        assert_eq!(
            r["list"],
            json!([{"id": list, "name": "Личные дела", "sortOrder": 3}])
        );

        // Задача в списке: удаление непустого — `taskListHasTask`; календарь удаляется только `Calendar/set`.
        let r = call(
            &env,
            "Task/set",
            json!({"accountId": acc, "create": {"t": {"taskListId": &list, "title": "Позвонить"}}}),
        )
        .await;
        assert!(r["created"]["t"]["id"].is_string(), "{r}");
        let r = call(
            &env,
            "TaskList/set",
            json!({"accountId": acc, "destroy": [&list, &cal]}),
        )
        .await;
        assert_eq!(r["notDestroyed"][&list]["type"], "taskListHasTask", "{r}");
        assert_eq!(r["notDestroyed"][&cal]["type"], "forbidden", "{r}");
        let r = call(
            &env,
            "TaskList/set",
            json!({"accountId": acc, "destroy": [&list], "onDestroyRemoveTasks": true}),
        )
        .await;
        assert_eq!(r["destroyed"], json!([list]), "{r}");
    }

    /// Задачи: создание, чтение, запрос, изменение, изменения, удаление.
    #[tokio::test]
    async fn tasks_crud() {
        let env = Env::new().await;
        let acc = env.account();
        let cal = create_calendar(&env, "Работа").await;
        let st =
            call(&env, "Task/get", json!({"accountId": acc, "ids": []})).await["state"].clone();
        let r = call(
            &env,
            "Task/set",
            json!({"accountId": acc, "create": {
                "a": {"title": "Отчёт", "description": "квартальный", "due": "2026-03-10T18:00:00"},
                "b": {"taskListId": &cal, "title": "Бумага", "progress": "completed"},
                "c": {"taskListId": "nope", "title": "Никуда"},
                "d": {"title": "UTC", "utcDue": "2026-03-10T15:00:00Z"}}}),
        )
        .await;
        let a = r["created"]["a"]["id"].as_str().expect("a").to_owned();
        let b = r["created"]["b"]["id"].as_str().expect("b").to_owned();
        assert_eq!(
            r["notCreated"]["c"]["properties"],
            json!(["taskListId"]),
            "{r}"
        );
        assert_eq!(r["notCreated"]["d"]["properties"], json!(["utcDue"]), "{r}");

        let g = call(
            &env,
            "Task/get",
            json!({"accountId": acc, "ids": [&a, "nope"]}),
        )
        .await;
        let t = &g["list"][0];
        assert_eq!(t["@type"], "Task", "{g}");
        assert_eq!(t["title"], "Отчёт", "{g}");
        assert_eq!(t["taskListId"], cal, "{g}");
        assert_eq!(t["due"], "2026-03-10T18:00:00", "{g}");
        assert_eq!(g["notFound"], json!(["nope"]), "{g}");
        // Задачи — не события.
        let e = call(&env, "CalendarEvent/get", json!({"accountId": acc})).await;
        assert_eq!(e["list"], json!([]), "{e}");

        let q = |filter: Value| {
            let env = &env;
            let acc = acc.clone();
            async move {
                call(
                    env,
                    "Task/query",
                    json!({"accountId": acc, "filter": filter, "sort": [{"property": "title"}]}),
                )
                .await["ids"]
                    .clone()
            }
        };
        assert_eq!(q(Value::Null).await, json!([b, a]));
        assert_eq!(q(json!({"text": "КВАРТАЛ"})).await, json!([a]));
        assert_eq!(q(json!({"progress": "completed"})).await, json!([b]));
        assert_eq!(
            q(json!({"operator": "NOT", "conditions": [{"progress": "completed"}]})).await,
            json!([a])
        );
        let r = call(
            &env,
            "Task/query",
            json!({"accountId": acc, "filter": {"colour": "red"}}),
        )
        .await;
        assert_eq!(r["type"], "unsupportedFilter", "{r}");

        let r = call(
            &env,
            "Task/set",
            json!({"accountId": acc, "update": {
                &a: {"progress": "completed", "title": "Отчёт сдан"},
                &b: {"taskListId": "other"}}}),
        )
        .await;
        assert!(r["updated"][&a].is_null(), "{r}");
        assert_eq!(
            r["notUpdated"][&b]["properties"],
            json!(["taskListId"]),
            "{r}"
        );
        let g = call(&env, "Task/get", json!({"accountId": acc, "ids": [&a]})).await;
        assert_eq!(g["list"][0]["progress"], "completed", "{g}");
        assert_eq!(g["list"][0]["title"], "Отчёт сдан", "{g}");

        let c = call(
            &env,
            "Task/changes",
            json!({"accountId": acc, "sinceState": st}),
        )
        .await;
        let mut created: Vec<String> = c["created"]
            .as_array()
            .expect("created")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        created.sort();
        let mut want = vec![a.clone(), b.clone()];
        want.sort();
        assert_eq!(created, want, "{c}");

        let r = call(&env, "Task/set", json!({"accountId": acc, "destroy": [&b]})).await;
        assert_eq!(r["destroyed"], json!([b]), "{r}");
        let qs = call(&env, "Task/query", json!({"accountId": acc})).await["queryState"].clone();
        let r = call(&env, "Task/set", json!({"accountId": acc, "destroy": [&a]})).await;
        assert_eq!(r["destroyed"], json!([a]), "{r}");
        let r = call(
            &env,
            "Task/queryChanges",
            json!({"accountId": acc, "sinceQueryState": qs}),
        )
        .await;
        assert_eq!(r["removed"], json!([a]), "{r}");
    }

    /// Приёмка: задача из JMAP видна в CalDAV (Thunderbird) и наоборот.
    #[tokio::test]
    async fn caldav_roundtrip() {
        let env = Env::new().await;
        let acc = env.account();
        let addr = dav(&env).await;
        let cal = create_calendar(&env, "Работа").await;
        let base = calendar_path(&env, &cal).await;

        // CalDAV → JMAP.
        let todo = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Mozilla.org/NONSGML Mozilla Calendar V1.1//EN\r\n\
                    BEGIN:VTODO\r\nUID:tb-1\r\nDTSTAMP:20260101T000000Z\r\nSUMMARY:Из Thunderbird\r\n\
                    DUE:20260305T170000Z\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let (status, text) = dav_request(addr, "PUT", &format!("{base}tb-1.ics"), todo).await;
        assert_eq!(status, 201, "{text}");
        let g = call(&env, "Task/get", json!({"accountId": acc})).await;
        assert_eq!(g["list"].as_array().map(Vec::len), Some(1), "{g}");
        assert_eq!(g["list"][0]["uid"], "tb-1", "{g}");
        assert_eq!(g["list"][0]["title"], "Из Thunderbird", "{g}");
        let id = g["list"][0]["id"].as_str().expect("id").to_owned();

        // Изменение из JMAP видно CalDAV.
        let r = call(
            &env,
            "Task/set",
            json!({"accountId": acc, "update": {&id: {"title": "Из JMAP"}}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        let (status, text) = dav_request(addr, "GET", &format!("{base}tb-1.ics"), "").await;
        assert_eq!(status, 200, "{text}");
        assert!(text.contains("BEGIN:VTODO"), "{text}");
        assert!(text.contains("SUMMARY:Из JMAP"), "{text}");
        assert!(text.contains("UID:tb-1"), "{text}");

        // JMAP → CalDAV.
        let r = call(
            &env,
            "Task/set",
            json!({"accountId": acc, "create": {"t": {"taskListId": &cal, "title": "Новая из JMAP"}}}),
        )
        .await;
        let created = r["created"]["t"]["id"].as_str().expect("id").to_owned();
        let objects = env
            .dav
            .list_objects(env.scope, cal.parse().expect("cal"))
            .await
            .expect("list");
        let name = objects
            .iter()
            .find(|o| o.id.to_string() == created)
            .map(|o| o.name.clone())
            .expect("object");
        let (status, text) = dav_request(addr, "GET", &format!("{base}{name}"), "").await;
        assert_eq!(status, 200, "{text}");
        assert!(text.contains("BEGIN:VTODO"), "{text}");
        assert!(text.contains("SUMMARY:Новая из JMAP"), "{text}");

        // Удаление в CalDAV видно JMAP.
        let st =
            call(&env, "Task/get", json!({"accountId": acc, "ids": []})).await["state"].clone();
        let (status, text) = dav_request(addr, "DELETE", &format!("{base}{name}"), "").await;
        assert_eq!(status, 204, "{text}");
        let c = call(
            &env,
            "Task/changes",
            json!({"accountId": acc, "sinceState": st}),
        )
        .await;
        assert_eq!(c["destroyed"], json!([created]), "{c}");
    }
}
