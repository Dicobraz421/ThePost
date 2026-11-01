//! Mailbox (RFC 8621 §2): `Mailbox/get`, `changes`, `query`, `queryChanges`, `set`.
//!
//! Папка — `tp_contracts::meta::Mailbox`; счётчики — `MailboxStore::status`, цепочки считаются по письмам папки.
//! `sortOrder` не хранится (всегда 0). Права — владельца учётной записи.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tp_contracts::meta::{Flag, Mailbox, NewMailbox, SpecialUse};
use tp_contracts::{Error, MailboxId};

use crate::core::error::{MethodError, MethodResult, changes_error, set_error};
use crate::core::request::{
    ChangesArgs, Comparator, GetArgs, QueryChangesArgs, QueryWindow, apply_window,
    changes_response, map_changes, object, parse_args, parse_state, query_changes_response, select,
    state_string,
};
use crate::core::{Ctx, thread_id};

/// Свойства-счётчики (`updatedProperties`, если изменились только письма папки).
const COUNT_PROPS: [&str; 4] = [
    "totalEmails",
    "unreadEmails",
    "totalThreads",
    "unreadThreads",
];

/// Свойства, которые задаёт сервер.
const SERVER_SET: [&str; 6] = [
    "id",
    "totalEmails",
    "unreadEmails",
    "totalThreads",
    "unreadThreads",
    "myRights",
];

fn role_name(r: SpecialUse) -> Value {
    serde_json::to_value(r).unwrap_or(Value::Null)
}

/// Права владельца; «Входящие» нельзя переименовать, системные папки — удалить (`MailboxStore`).
fn rights(role: Option<SpecialUse>) -> Value {
    json!({
        "mayReadItems": true, "mayAddItems": true, "mayRemoveItems": true, "maySetSeen": true,
        "maySetKeywords": true, "mayCreateChild": true, "mayRename": role != Some(SpecialUse::Inbox),
        "mayDelete": role.is_none(), "maySubmit": true,
    })
}

/// Объект Mailbox; `threads` — считать `totalThreads`/`unreadThreads` (читает письма папки).
async fn to_json(ctx: &Ctx<'_>, mb: &Mailbox, threads: bool) -> MethodResult<Map<String, Value>> {
    let st = ctx.jmap.d.mailboxes.status(ctx.scope, mb.id).await?;
    let (mut all, mut unread) = (HashSet::new(), HashSet::new());
    if threads {
        for m in ctx.jmap.messages_in(ctx.scope, Some(mb.id)).await? {
            let t = thread_id(&m);
            if !m.flags.contains(&Flag::Seen) {
                unread.insert(t.clone());
            }
            all.insert(t);
        }
    }
    Ok(object(json!({
        "id": mb.id.to_string(),
        "name": mb.name,
        "parentId": mb.parent.map(|p| p.to_string()),
        "role": mb.role.map(role_name),
        "sortOrder": 0,
        "totalEmails": st.messages,
        "unreadEmails": st.unseen,
        "totalThreads": all.len(),
        "unreadThreads": unread.len(),
        "myRights": rights(mb.role),
        "isSubscribed": mb.subscribed,
    })))
}

/// `Mailbox/get`.
pub(crate) async fn get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let state = ctx.state().await?;
    let all = ctx.jmap.d.mailboxes.list(ctx.scope).await?;
    let props = a.properties.as_deref();
    let threads = props.is_none_or(|p| {
        p.iter()
            .any(|x| x == "totalThreads" || x == "unreadThreads")
    });
    let mut wanted = Vec::new();
    let mut not_found = Vec::new();
    match &a.ids {
        None => wanted.extend(all.iter()),
        Some(ids) => {
            for id in ids {
                let real = ctx.resolve_id(id);
                match all.iter().find(|m| m.id.to_string() == real) {
                    Some(m) => wanted.push(m),
                    None => not_found.push(id.clone()),
                }
            }
        }
    }
    let mut list = Vec::new();
    for mb in wanted {
        list.push(Value::Object(select(
            to_json(ctx, mb, threads).await?,
            props,
        )));
    }
    Ok(object(json!({
        "accountId": a.account_id,
        "state": state_string(state),
        "list": list,
        "notFound": not_found,
    })))
}

/// `Mailbox/changes`: изменения папок по журналу; папки, у которых изменились только письма, —
/// в `updated` с `updatedProperties` = счётчики.
pub(crate) async fn changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: ChangesArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let since = parse_state(&a.since_state)?;
    let limit = a.limit(ctx.jmap.cfg.max_changes)?;
    let d = &ctx.jmap.d;
    let c = d
        .changes
        .mailbox_changes(ctx.scope, since, limit)
        .await
        .map_err(changes_error)?;
    let mut c = map_changes(c, ToString::to_string);
    let listed: HashSet<String> = c
        .created
        .iter()
        .chain(&c.updated)
        .chain(&c.destroyed)
        .cloned()
        .collect();
    let mut counts = Vec::new();
    for mb in d.mailboxes.list(ctx.scope).await? {
        let id = mb.id.to_string();
        if listed.contains(&id) {
            continue;
        }
        let m = d
            .changes
            .message_changes(ctx.scope, Some(mb.id), since, 1)
            .await
            .map_err(changes_error)?;
        if !(m.created.is_empty() && m.updated.is_empty() && m.destroyed.is_empty()) {
            counts.push(id);
        }
    }
    let props = if c.updated.is_empty() && !counts.is_empty() {
        json!(COUNT_PROPS)
    } else {
        Value::Null
    };
    c.updated.extend(counts);
    let mut out = changes_response(&a.account_id, &a.since_state, &c);
    out.insert("updatedProperties".into(), props);
    Ok(out)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryArgs {
    account_id: String,
    filter: Option<Value>,
    sort: Option<Vec<Comparator>>,
    #[serde(default)]
    sort_as_tree: bool,
    #[serde(default)]
    filter_as_tree: bool,
    #[serde(flatten)]
    window: QueryWindow,
}

fn unsupported_filter(why: &str) -> MethodError {
    MethodError::with("unsupportedFilter", why)
}

/// Условие `FilterCondition` или `FilterOperator` для папки.
fn matches(f: &Value, mb: &Mailbox) -> MethodResult<bool> {
    let Value::Object(m) = f else {
        return Err(unsupported_filter("filter must be an object"));
    };
    if let Some(op) = m.get("operator") {
        let conds = m
            .get("conditions")
            .and_then(Value::as_array)
            .ok_or_else(|| unsupported_filter("conditions"))?;
        let rs = conds
            .iter()
            .map(|c| matches(c, mb))
            .collect::<MethodResult<Vec<bool>>>()?;
        return match op.as_str() {
            Some("AND") => Ok(rs.iter().all(|x| *x)),
            Some("OR") => Ok(rs.iter().any(|x| *x)),
            Some("NOT") => Ok(!rs.iter().any(|x| *x)),
            _ => Err(unsupported_filter("operator")),
        };
    }
    for (k, v) in m {
        let ok = match (k.as_str(), v) {
            ("parentId", Value::Null) => mb.parent.is_none(),
            ("parentId", Value::String(s)) => mb.parent.is_some_and(|p| p.to_string() == *s),
            ("name", Value::String(s)) => mb.name.to_lowercase().contains(&s.to_lowercase()),
            ("role", Value::Null) => mb.role.is_none(),
            ("role", Value::String(s)) => mb.role.map(role_name) == Some(json!(s)),
            ("hasAnyRole", Value::Bool(b)) => mb.role.is_some() == *b,
            ("isSubscribed", Value::Bool(b)) => mb.subscribed == *b,
            _ => return Err(unsupported_filter(k)),
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Идентификаторы папок, подходящих под запрос, в порядке сортировки.
fn run_query(all: &[Mailbox], a: &QueryArgs) -> MethodResult<Vec<String>> {
    let mut hit: HashSet<MailboxId> = HashSet::new();
    for mb in all {
        if a.filter.as_ref().map_or(Ok(true), |f| matches(f, mb))? {
            hit.insert(mb.id);
        }
    }
    let by_id: HashMap<MailboxId, &Mailbox> = all.iter().map(|m| (m.id, m)).collect();
    if a.filter_as_tree {
        // Папка остаётся, только если подходят и все её предки.
        let ok = |id: MailboxId| {
            let mut cur = by_id.get(&id).and_then(|m| m.parent);
            for _ in 0..all.len() {
                match cur {
                    Some(p) if hit.contains(&p) => cur = by_id.get(&p).and_then(|m| m.parent),
                    Some(p) => return !by_id.contains_key(&p),
                    None => return true,
                }
            }
            true
        };
        let keep: HashSet<MailboxId> = hit.iter().copied().filter(|id| ok(*id)).collect();
        hit = keep;
    }
    let comparators = a.sort.clone().unwrap_or_else(|| {
        vec![Comparator {
            property: "name".into(),
            is_ascending: true,
            collation: None,
            keyword: None,
        }]
    });
    for c in &comparators {
        if c.property != "name" && c.property != "sortOrder" {
            return Err(MethodError::with("unsupportedSort", c.property.clone()));
        }
    }
    let cmp = |x: &&Mailbox, y: &&Mailbox| {
        for c in &comparators {
            let o = match c.property.as_str() {
                "name" => x.name.to_lowercase().cmp(&y.name.to_lowercase()),
                _ => Ordering::Equal,
            };
            let o = if c.is_ascending { o } else { o.reverse() };
            if o != Ordering::Equal {
                return o;
            }
        }
        x.id.cmp(&y.id)
    };
    let mut sorted: Vec<&Mailbox> = all.iter().filter(|m| hit.contains(&m.id)).collect();
    sorted.sort_by(cmp);
    if !a.sort_as_tree {
        return Ok(sorted.iter().map(|m| m.id.to_string()).collect());
    }
    // Дерево: каждая папка сразу после родителя; папки без родителя в результате — корни.
    fn visit(
        at: Option<MailboxId>,
        sorted: &[&Mailbox],
        roots: bool,
        out: &mut Vec<String>,
        seen: &mut HashSet<MailboxId>,
    ) {
        for m in sorted {
            let child = if roots {
                m.parent.is_none_or(|p| !sorted.iter().any(|s| s.id == p))
            } else {
                m.parent == at
            };
            if child && seen.insert(m.id) {
                out.push(m.id.to_string());
                visit(Some(m.id), sorted, false, out, seen);
            }
        }
    }
    let mut out = Vec::new();
    visit(None, &sorted, true, &mut out, &mut HashSet::new());
    Ok(out)
}

/// `Mailbox/query`.
pub(crate) async fn query(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: QueryArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let state = ctx.state().await?;
    let all = ctx.jmap.d.mailboxes.list(ctx.scope).await?;
    let ids = run_query(&all, &a)?;
    let mut out = object(json!({
        "accountId": a.account_id,
        "queryState": state_string(state),
        "canCalculateChanges": true,
    }));
    apply_window(&ids, &a.window, ctx.jmap.cfg.max_query_limit, &mut out)?;
    Ok(out)
}

/// `Mailbox/queryChanges` по журналу изменений папок.
pub(crate) async fn query_changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let qc: QueryChangesArgs = parse_args(args.clone())?;
    ctx.check_account(&qc.account_id)?;
    let mut q = args;
    q.retain(|k, _| {
        matches!(
            k.as_str(),
            "accountId" | "filter" | "sort" | "sortAsTree" | "filterAsTree"
        )
    });
    let a: QueryArgs = parse_args(q)?;
    let since = parse_state(&qc.since_query_state)?;
    let limit = qc.limit(ctx.jmap.cfg.max_changes)?;
    let c = ctx
        .jmap
        .d
        .changes
        .mailbox_changes(ctx.scope, since, limit.saturating_add(1))
        .await
        .map_err(changes_error)?;
    let c = map_changes(c, ToString::to_string);
    let all = ctx.jmap.d.mailboxes.list(ctx.scope).await?;
    let ids = run_query(&all, &a)?;
    query_changes_response(&qc, &ids, &c, limit)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetArgs {
    account_id: String,
    if_in_state: Option<String>,
    create: Option<Map<String, Value>>,
    update: Option<Map<String, Value>>,
    destroy: Option<Vec<String>>,
    #[serde(default)]
    on_destroy_remove_emails: bool,
}

/// Папка по ссылке: id или `#cid` созданной в запросе.
fn mailbox_ref(ctx: &Ctx<'_>, s: &str) -> Option<MailboxId> {
    MailboxId::from_str(ctx.resolve_id(s)).ok()
}

/// Ошибка хранилища для `/set`: ошибка объекта или (недоступность, сбой) ошибка метода.
fn store_set_error(e: Error) -> MethodResult<Value> {
    Ok(match e {
        Error::Conflict => set_error("invalidProperties", Some("name already exists"), &["name"]),
        Error::Invalid(m) => set_error("invalidProperties", Some(&m), &[]),
        Error::Forbidden => set_error("forbidden", None, &[]),
        Error::NotFound => set_error("invalidProperties", Some("parent not found"), &["parentId"]),
        e => return Err(e.into()),
    })
}

fn invalid(props: &[&str]) -> Value {
    set_error("invalidProperties", None, props)
}

/// Разобранный объект создания: имя, родитель (`Err` — ссылка на ещё не созданную папку), назначение, подписка.
type Create = (
    String,
    Result<Option<MailboxId>, String>,
    Option<SpecialUse>,
    bool,
);

fn parse_create(ctx: &Ctx<'_>, obj: &Value) -> Result<Create, Value> {
    let Value::Object(m) = obj else {
        return Err(invalid(&[]));
    };
    let mut name = None;
    let mut parent = Ok(None);
    let mut role = None;
    let mut subscribed = false;
    for (k, v) in m {
        match (k.as_str(), v) {
            ("name", Value::String(s)) if !s.is_empty() => name = Some(s.clone()),
            ("parentId", Value::Null) => {}
            ("parentId", Value::String(s)) => {
                parent = match mailbox_ref(ctx, s) {
                    Some(id) => Ok(Some(id)),
                    None if s.starts_with('#') => Err(s.clone()),
                    None => return Err(invalid(&["parentId"])),
                }
            }
            ("role", v) => {
                role = serde_json::from_value::<Option<SpecialUse>>(v.clone())
                    .map_err(|_| invalid(&["role"]))?;
            }
            ("isSubscribed", Value::Bool(b)) => subscribed = *b,
            ("sortOrder", v) if v.as_u64() == Some(0) => {}
            (k, _) => return Err(invalid(&[k])),
        }
    }
    let name = name.ok_or_else(|| invalid(&["name"]))?;
    Ok((name, parent, role, subscribed))
}

/// `Mailbox/set`: создание (ссылки `#cid` на родителя — в любом порядке), изменение, удаление.
pub(crate) async fn set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: SetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let n = a.create.as_ref().map_or(0, Map::len)
        + a.update.as_ref().map_or(0, Map::len)
        + a.destroy.as_ref().map_or(0, Vec::len);
    if n > ctx.jmap.cfg.max_objects_in_set as usize {
        return Err(MethodError::new("requestTooLarge"));
    }
    let old = state_string(ctx.state().await?);
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }
    let store = ctx.jmap.d.mailboxes.clone();
    let scope = ctx.scope;
    let (mut created, mut not_created) = (Map::new(), Map::new());
    let mut pending: Vec<(String, Value)> = a.create.unwrap_or_default().into_iter().collect();
    // Проходы, пока создаются папки: родитель `#cid` может быть создан позже в этом же вызове.
    loop {
        let mut progress = false;
        let mut rest = Vec::new();
        for (cid, obj) in pending {
            let (name, parent, role, subscribed) = match parse_create(ctx, &obj) {
                Ok(c) => c,
                Err(e) => {
                    not_created.insert(cid, e);
                    continue;
                }
            };
            let Ok(parent) = parent else {
                rest.push((cid, obj));
                continue;
            };
            progress = true;
            match store.create(scope, NewMailbox { parent, name, role }).await {
                Ok(mb) => {
                    if subscribed {
                        store.set_subscribed(scope, mb.id, true).await?;
                    }
                    let mut o = to_json(ctx, &mb, false).await?;
                    o.retain(|k, _| SERVER_SET.contains(&k.as_str()) || k == "sortOrder");
                    o.insert("isSubscribed".into(), json!(subscribed));
                    ctx.created.insert(cid.clone(), mb.id.to_string());
                    created.insert(cid, Value::Object(o));
                }
                Err(e) => {
                    not_created.insert(cid, store_set_error(e)?);
                }
            }
        }
        pending = rest;
        if !progress || pending.is_empty() {
            break;
        }
    }
    for (cid, _) in pending {
        not_created.insert(cid, invalid(&["parentId"]));
    }

    let (mut updated, mut not_updated) = (Map::new(), Map::new());
    for (id, patch) in a.update.unwrap_or_default() {
        match update_one(ctx, &id, &patch).await? {
            Ok(()) => {
                updated.insert(id, Value::Null);
            }
            Err(e) => {
                not_updated.insert(id, e);
            }
        }
    }

    let (mut destroyed, mut not_destroyed) = (Vec::new(), Map::new());
    for id in a.destroy.unwrap_or_default() {
        match destroy_one(ctx, &id, a.on_destroy_remove_emails).await? {
            Ok(()) => destroyed.push(id),
            Err(e) => {
                not_destroyed.insert(id, e);
            }
        }
    }
    let new = state_string(ctx.state().await?);
    let or_null = |m: Map<String, Value>| {
        if m.is_empty() {
            Value::Null
        } else {
            Value::Object(m)
        }
    };
    Ok(object(json!({
        "accountId": a.account_id,
        "oldState": old,
        "newState": new,
        "created": or_null(created),
        "updated": or_null(updated),
        "destroyed": if destroyed.is_empty() { Value::Null } else { json!(destroyed) },
        "notCreated": or_null(not_created),
        "notUpdated": or_null(not_updated),
        "notDestroyed": or_null(not_destroyed),
    })))
}

/// Изменение одной папки: `Ok(Err(SetError))` — отказ по объекту.
async fn update_one(ctx: &Ctx<'_>, id: &str, patch: &Value) -> MethodResult<Result<(), Value>> {
    let d = &ctx.jmap.d;
    let Some(mb) = (match mailbox_ref(ctx, id) {
        Some(mid) => d.mailboxes.get(ctx.scope, mid).await?,
        None => None,
    }) else {
        return Ok(Err(set_error("notFound", None, &[])));
    };
    let Value::Object(patch) = patch else {
        return Ok(Err(invalid(&[])));
    };
    let (mut name, mut parent, mut subscribe) = (mb.name.clone(), mb.parent, None);
    for (k, v) in patch {
        match (k.as_str(), v) {
            ("name", Value::String(s)) if !s.is_empty() => name = s.clone(),
            ("parentId", Value::Null) => parent = None,
            ("parentId", Value::String(s)) => match mailbox_ref(ctx, s) {
                Some(p) => parent = Some(p),
                None => return Ok(Err(invalid(&["parentId"]))),
            },
            ("isSubscribed", Value::Bool(b)) => subscribe = Some(*b),
            ("sortOrder", v) if v.as_u64() == Some(0) => {}
            ("role", v) if *v == mb.role.map_or(Value::Null, role_name) => {}
            (k, _) => return Ok(Err(invalid(&[k]))),
        }
    }
    if (name != mb.name || parent != mb.parent)
        && let Err(e) = d.mailboxes.rename(ctx.scope, mb.id, parent, &name).await
    {
        return Ok(Err(store_set_error(e)?));
    }
    if let Some(on) = subscribe {
        d.mailboxes.set_subscribed(ctx.scope, mb.id, on).await?;
    }
    Ok(Ok(()))
}

/// Удаление одной папки: письма — в корзину только с `onDestroyRemoveEmails`.
async fn destroy_one(
    ctx: &Ctx<'_>,
    id: &str,
    remove_emails: bool,
) -> MethodResult<Result<(), Value>> {
    let d = &ctx.jmap.d;
    let Some(mb) = (match mailbox_ref(ctx, id) {
        Some(mid) => d.mailboxes.get(ctx.scope, mid).await?,
        None => None,
    }) else {
        return Ok(Err(set_error("notFound", None, &[])));
    };
    if !remove_emails && d.mailboxes.status(ctx.scope, mb.id).await?.messages > 0 {
        return Ok(Err(set_error("mailboxHasEmail", None, &[])));
    }
    Ok(match d.mailboxes.delete(ctx.scope, mb.id, false).await {
        Ok(()) => Ok(()),
        Err(Error::Conflict) => Err(set_error("mailboxHasChild", None, &[])),
        Err(Error::Forbidden) => Err(set_error("forbidden", None, &[])),
        Err(Error::NotFound) => Err(set_error("notFound", None, &[])),
        Err(e) => return Err(e.into()),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tp_contracts::meta::{Flag, SpecialUse};

    use crate::testenv::{Env, mail};

    fn names(list: &Value) -> Vec<String> {
        let mut v: Vec<String> = list
            .as_array()
            .expect("list")
            .iter()
            .map(|m| m["name"].as_str().unwrap_or_default().to_owned())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn get_counts_and_properties() {
        let env = Env::new().await;
        let inbox = env.mailbox(SpecialUse::Inbox);
        let a = mail("Тема", "a@example.com", "Message-ID: <r1@x>\r\n", "раз");
        let b = mail(
            "Re: Тема",
            "b@example.com",
            "In-Reply-To: <r1@x>\r\nReferences: <r1@x>\r\n",
            "два",
        );
        env.deliver(inbox, &a, [Flag::Seen].into()).await;
        env.deliver(inbox, &b, Default::default()).await;
        let acc = env.account();
        let r = env
            .call("Mailbox/get", json!({"accountId": acc}), "Mailbox/get")
            .await;
        assert_eq!(
            names(&r["list"]),
            ["Drafts", "INBOX", "Junk", "Sent", "Trash"]
        );
        assert!(r["state"].is_string());
        let id = inbox.to_string();
        let r = env
            .call(
                "Mailbox/get",
                json!({"accountId": acc, "ids": [id, "nope"], "properties": ["name", "role", "totalEmails",
                    "unreadEmails", "totalThreads", "unreadThreads", "myRights"]}),
                "Mailbox/get",
            )
            .await;
        let m = &r["list"][0];
        assert_eq!(m["role"], "inbox");
        assert_eq!(m["totalEmails"], 2);
        assert_eq!(m["unreadEmails"], 1);
        assert_eq!(m["totalThreads"], 1);
        assert_eq!(m["unreadThreads"], 1);
        assert_eq!(m["myRights"]["mayRename"], false);
        assert!(m.get("isSubscribed").is_none());
        assert_eq!(r["notFound"], json!(["nope"]));
        let r = env
            .calls(json!([["Mailbox/get", {"accountId": "other"}, "x"]]))
            .await;
        assert_eq!(r[0], json!(["error", {"type": "accountNotFound"}, "x"]));
    }

    #[tokio::test]
    async fn set_create_update_destroy() {
        let env = Env::new().await;
        let acc = env.account();
        // «child» обрабатывается раньше «parent» по порядку ключей: ссылка на родителя ждёт второго прохода.
        let r = env
            .calls(json!([
                ["Mailbox/set", {"accountId": acc, "create": {
                    "child": {"name": "Отчёты", "parentId": "#parent"},
                    "parent": {"name": "Проекты", "isSubscribed": true},
                    "bad": {"name": "", "totalEmails": 3},
                    "orphan": {"name": "X", "parentId": "#missing"}
                }}, "s"],
                ["Mailbox/get", {"accountId": acc, "ids": ["#child"], "properties": ["name", "parentId"]}, "g"]
            ]))
            .await;
        let set = &r[0][1];
        let parent = set["created"]["parent"]["id"]
            .as_str()
            .expect("id")
            .to_owned();
        let child = set["created"]["child"]["id"]
            .as_str()
            .expect("id")
            .to_owned();
        assert_eq!(set["created"]["parent"]["isSubscribed"], true);
        assert_eq!(set["created"]["parent"]["totalEmails"], 0);
        assert_eq!(set["notCreated"]["bad"]["type"], "invalidProperties");
        assert_eq!(
            set["notCreated"]["orphan"]["properties"],
            json!(["parentId"])
        );
        assert_ne!(set["oldState"], set["newState"]);
        assert_eq!(r[1][1]["list"][0]["parentId"], parent);

        let inbox = env.mailbox(SpecialUse::Inbox).to_string();
        let trash = env.mailbox(SpecialUse::Trash).to_string();
        env.deliver(
            env.mailbox(SpecialUse::Inbox),
            &mail("s", "a@x.org", "", "b"),
            Default::default(),
        )
        .await;
        let r = env
            .call(
                "Mailbox/set",
                json!({"accountId": acc,
                    "update": {
                        child.clone(): {"name": "Отчёты-2026", "isSubscribed": true},
                        inbox.clone(): {"name": "Входящие"},
                        trash.clone(): {"role": "inbox"}
                    },
                    "destroy": [parent.clone(), inbox.clone(), "nope"]}),
                "Mailbox/set",
            )
            .await;
        assert_eq!(r["updated"], json!({child.clone(): null}));
        assert_eq!(r["notUpdated"][&inbox]["type"], "forbidden");
        assert_eq!(r["notUpdated"][&trash]["properties"], json!(["role"]));
        assert_eq!(r["notDestroyed"][&parent]["type"], "mailboxHasChild");
        assert_eq!(r["notDestroyed"][&inbox]["type"], "mailboxHasEmail");
        assert_eq!(r["notDestroyed"]["nope"]["type"], "notFound");

        let r = env
            .call(
                "Mailbox/set",
                json!({"accountId": acc, "destroy": [child.clone(), trash.clone()]}),
                "Mailbox/set",
            )
            .await;
        assert_eq!(r["destroyed"], json!([child]));
        assert_eq!(r["notDestroyed"][&trash]["type"], "forbidden");
        let r = env
            .calls(json!([["Mailbox/set", {"accountId": acc, "ifInState": "0", "destroy": [parent]}, "s"]]))
            .await;
        assert_eq!(r[0][1]["type"], "stateMismatch");
    }

    #[tokio::test]
    async fn changes_with_count_updates() {
        let env = Env::new().await;
        let acc = env.account();
        let s0 = env
            .call(
                "Mailbox/get",
                json!({"accountId": acc, "ids": []}),
                "Mailbox/get",
            )
            .await["state"]
            .clone();
        let r = env
            .call(
                "Mailbox/set",
                json!({"accountId": acc, "create": {"a": {"name": "A"}}}),
                "Mailbox/set",
            )
            .await;
        let a = r["created"]["a"]["id"].clone();
        let s1 = r["newState"].clone();
        let c = env
            .call(
                "Mailbox/changes",
                json!({"accountId": acc, "sinceState": s0}),
                "Mailbox/changes",
            )
            .await;
        assert_eq!(c["created"], json!([a]));
        assert_eq!(c["newState"], s1);
        assert_eq!(c["hasMoreChanges"], false);
        let inbox = env.mailbox(SpecialUse::Inbox);
        env.deliver(inbox, &mail("s", "a@x.org", "", "b"), Default::default())
            .await;
        let c = env
            .call(
                "Mailbox/changes",
                json!({"accountId": acc, "sinceState": s1}),
                "Mailbox/changes",
            )
            .await;
        // Папка в `updated`: журнал папок или изменения её писем (тогда `updatedProperties` — счётчики).
        assert_eq!(c["updated"], json!([inbox.to_string()]));
        assert!(
            c["updatedProperties"].is_null()
                || c["updatedProperties"]
                    == json!([
                        "totalEmails",
                        "unreadEmails",
                        "totalThreads",
                        "unreadThreads"
                    ])
        );
        let s2 = c["newState"].clone();
        let c = env
            .call(
                "Mailbox/changes",
                json!({"accountId": acc, "sinceState": s2}),
                "Mailbox/changes",
            )
            .await;
        assert_eq!(c["updated"], json!([]));
        assert_eq!(c["newState"], s2);
        let current = s2.as_str().expect("state").parse().expect("modseq");
        env.meta
            .truncate_changelog(env.scope, tp_contracts::ModSeq(current))
            .expect("truncate");
        let r = env
            .calls(json!([["Mailbox/changes", {"accountId": acc, "sinceState": s0}, "c"]]))
            .await;
        assert_eq!(r[0][1]["type"], "cannotCalculateChanges");
        let r = env
            .calls(json!([["Mailbox/changes", {"accountId": acc, "sinceState": "zz"}, "c"]]))
            .await;
        assert_eq!(r[0][1]["type"], "cannotCalculateChanges");
    }

    #[tokio::test]
    async fn query_and_query_changes() {
        let env = Env::new().await;
        let acc = env.account();
        let r = env
            .call(
                "Mailbox/set",
                json!({"accountId": acc, "create": {"p": {"name": "Archive2"}, "c": {"name": "child", "parentId": "#p"}}}),
                "Mailbox/set",
            )
            .await;
        let p = r["created"]["p"]["id"].clone();
        let c = r["created"]["c"]["id"].clone();
        let q = env
            .call(
                "Mailbox/query",
                json!({"accountId": acc, "filter": {"hasAnyRole": false}, "sortAsTree": true, "calculateTotal": true}),
                "Mailbox/query",
            )
            .await;
        assert_eq!(q["ids"], json!([p, c]));
        assert_eq!(q["total"], 2);
        assert_eq!(q["canCalculateChanges"], true);
        let q2 = env
            .call(
                "Mailbox/query",
                json!({"accountId": acc, "filter": {"operator": "AND", "conditions": [
                    {"name": "CHI"}, {"operator": "NOT", "conditions": [{"parentId": null}]}]}}),
                "Mailbox/query",
            )
            .await;
        assert_eq!(q2["ids"], json!([c]));
        let roles = env
            .call(
                "Mailbox/query",
                json!({"accountId": acc, "filter": {"role": "trash"}, "filterAsTree": true}),
                "Mailbox/query",
            )
            .await;
        assert_eq!(
            roles["ids"],
            json!([env.mailbox(SpecialUse::Trash).to_string()])
        );
        let r = env
            .calls(json!([
                ["Mailbox/query", {"accountId": acc, "filter": {"color": "red"}}, "a"],
                ["Mailbox/query", {"accountId": acc, "sort": [{"property": "color"}]}, "b"],
                ["Mailbox/query", {"accountId": acc, "anchor": "nope"}, "c"]
            ]))
            .await;
        assert_eq!(r[0][1]["type"], "unsupportedFilter");
        assert_eq!(r[1][1]["type"], "unsupportedSort");
        assert_eq!(r[2][1]["type"], "anchorNotFound");

        let cid = c.as_str().expect("id").to_owned();
        env.call(
            "Mailbox/set",
            json!({"accountId": acc, "update": {cid: {"name": "a-child"}}}),
            "Mailbox/set",
        )
        .await;
        let qc = env
            .call(
                "Mailbox/queryChanges",
                json!({"accountId": acc, "filter": {"hasAnyRole": false}, "sinceQueryState": q["queryState"],
                    "calculateTotal": true}),
                "Mailbox/queryChanges",
            )
            .await;
        assert_eq!(qc["removed"], json!([c]));
        // Без sortAsTree порядок — по имени: «a-child» раньше «Archive2».
        assert_eq!(qc["added"], json!([{"id": c, "index": 0}]));
        assert_eq!(qc["total"], 2);
        let r = env
            .calls(json!([["Mailbox/queryChanges", {"accountId": acc, "sinceQueryState": q["queryState"],
                "maxChanges": 0}, "x"]]))
            .await;
        assert_eq!(r[0][1]["type"], "invalidArguments");
    }
}
