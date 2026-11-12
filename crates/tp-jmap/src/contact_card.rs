//! JMAP Contacts — карточки (RFC 9610 §3; JMAP-26): `ContactCard/get`, `/changes`, `/query`, `/queryChanges`,
//! `/set`, `/copy` поверх [`DavStore`].
//!
//! Хранится vCard — тот же объект, что читает и пишет CardDAV; JSContact ↔ vCard — calcard, индекс объекта —
//! `tp_dav::index::card`. Карточка — в одной адресной книге (`maxAddressBooksPerCard` = 1): перенос между книгами
//! сменой `addressBookIds` не поддерживается (контракт не переносит объект с сохранением `id`). `/copy` — между
//! учётными записями, а доступна только своя: чужая — `fromAccountNotFound`. Запрос читает карточки книг и
//! фильтрует в памяти (фильтры RFC 9610 §3.3.1; `text` — имя, псевдонимы, организации, адреса, телефоны, сервисы,
//! почтовые адреса, заметки).

use std::cmp::Ordering;
use std::str::FromStr;

use calcard::jscontact::JSContact;
use calcard::vcard::VCard;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tp_contracts::Error;
use tp_contracts::dav::{
    CollectionId, DavObject, DavObjectId, DavStore, DeleteOutcome, ObjectIndex, Precondition,
    PutOutcome,
};
use tp_contracts::meta::AccountScope;

use crate::contacts::{KIND, default_book, state, store_error};
use crate::core::Ctx;
use crate::core::error::{MethodError, MethodResult, changes_error, set_error};
use crate::core::request::{
    ChangesArgs, Comparator, GetArgs, QueryChangesArgs, QueryWindow, SetArgs, SetOutcome,
    apply_window, changes_response, map_changes, object, parse_args, parse_state,
    query_changes_response, select,
};
use crate::email_get::utc_date;

/// Карточка JMAP из объекта хранилища: JSContact из vCard и свойства JMAP (`id`, `addressBookIds`).
/// Неразбираемое тело (хранилище его не проверяет) — карточка только с `uid` из индекса.
fn card_json(obj: &DavObject) -> Map<String, Value> {
    let mut m = VCard::parse(&obj.body)
        .ok()
        .and_then(|v| serde_json::to_value(v.into_jscontact::<String, String>()).ok())
        .map(object)
        .unwrap_or_default();
    m.entry("@type").or_insert_with(|| json!("Card"));
    m.entry("version").or_insert_with(|| json!("1.0"));
    m.entry("uid").or_insert_with(|| json!(obj.index.uid));
    m.insert("id".into(), json!(obj.id.to_string()));
    let mut books = Map::new();
    books.insert(obj.collection.to_string(), json!(true));
    m.insert("addressBookIds".into(), Value::Object(books));
    m
}

fn invalid(why: &str, props: &[&str]) -> Value {
    set_error("invalidProperties", Some(why), props)
}

/// vCard и индекс из карточки JMAP (свойства JMAP отбрасываются); ошибка — `invalidProperties`.
fn to_vcard(mut card: Map<String, Value>) -> Result<(String, ObjectIndex), Value> {
    card.remove("id");
    card.remove("addressBookIds");
    let text = Value::Object(card).to_string();
    let js = JSContact::<String, String>::parse(&text)
        .map_err(|e| invalid(&format!("invalid JSContact: {e}"), &[]))?;
    let vcard = js
        .into_vcard()
        .ok_or_else(|| invalid("JSContact cannot be converted to vCard", &[]))?;
    let body = vcard.to_string();
    let index = tp_dav::index::card(&body).map_err(|e| invalid(&e.to_string(), &["uid"]))?;
    Ok((body, index))
}

/// Единственная адресная книга из `addressBookIds`; иначе — `None`.
pub(crate) fn single_book(v: &Value) -> Option<&str> {
    let m = v.as_object()?;
    let mut on = m.iter().filter(|(_, v)| **v == Value::Bool(true));
    let (id, _) = on.next()?;
    (on.next().is_none() && m.values().all(|v| v.is_boolean() || v.is_null())).then_some(id)
}

/// Карточки книг `books` (в порядке книг и имён ресурсов).
pub(crate) async fn load(
    store: &dyn DavStore,
    scope: AccountScope,
    books: &[CollectionId],
) -> MethodResult<Vec<DavObject>> {
    let mut out = Vec::new();
    for b in books {
        let names: Vec<String> = match store.list_objects(scope, *b).await {
            Ok(refs) => refs.into_iter().map(|r| r.name).collect(),
            // Книгу удалили между запросами.
            Err(Error::NotFound) => continue,
            Err(e) => return Err(e.into()),
        };
        match store.multiget(scope, *b, &names).await {
            Ok(objs) => out.extend(objs),
            Err(Error::NotFound) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(out)
}

async fn book_ids(store: &dyn DavStore, scope: AccountScope) -> MethodResult<Vec<CollectionId>> {
    Ok(store
        .list_collections(scope, KIND)
        .await?
        .into_iter()
        .map(|b| b.id)
        .collect())
}

/// Объект карточки по `id` JMAP; чужой формат, нет объекта или объект календаря — `None`.
async fn find(
    store: &dyn DavStore,
    scope: AccountScope,
    id: &str,
) -> MethodResult<Option<DavObject>> {
    let Ok(oid) = DavObjectId::from_str(id) else {
        return Ok(None);
    };
    let obj = store.objects_by_id(scope, &[oid]).await?.into_iter().next();
    Ok(obj.filter(|o| o.index.component.fits(KIND)))
}

/// `ContactCard/get`.
pub(crate) async fn get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let store = ctx.jmap.dav_store()?;
    let st = state(store.as_ref(), ctx.scope).await?;
    let props = a.properties.as_deref();
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    match &a.ids {
        None => {
            let books = book_ids(store.as_ref(), ctx.scope).await?;
            let all = load(store.as_ref(), ctx.scope, &books).await?;
            list.extend(all.iter().map(|o| select(card_json(o), props)));
        }
        Some(ids) => {
            let mut seen: Vec<&str> = Vec::new();
            for id in ids {
                let id = ctx.resolve_id(id);
                if seen.contains(&id) {
                    continue;
                }
                seen.push(id);
                match find(store.as_ref(), ctx.scope, id).await? {
                    Some(o) => list.push(select(card_json(&o), props)),
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

/// `ContactCard/changes` по журналу объектов адресных книг.
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
        .account_changes(ctx.scope, KIND, since, limit)
        .await
        .map_err(changes_error)?;
    let c = map_changes(c, ToString::to_string);
    Ok(changes_response(&a.account_id, &a.since_state, &c))
}

// ---------- Запрос ----------

/// Ключи JSContact без текста для поиска (типы, виды, контексты).
const NOT_TEXT: [&str; 6] = ["@type", "kind", "contexts", "features", "pref", "phonetic"];

/// Строки значения (рекурсивно), в нижнем регистре.
fn strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.to_lowercase()),
        Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
        Value::Object(m) => m
            .iter()
            .filter(|(k, _)| !NOT_TEXT.contains(&k.as_str()))
            .for_each(|(_, x)| strings(x, out)),
        _ => {}
    }
}

/// Есть ли `needle` (в нижнем регистре) в строках свойств `props` карточки.
pub(crate) fn contains(card: &Map<String, Value>, props: &[&str], needle: &str) -> bool {
    let mut all = Vec::new();
    for p in props {
        if let Some(v) = card.get(*p) {
            strings(v, &mut all);
        }
    }
    all.iter().any(|s| s.contains(needle))
}

/// Компонент имени вида `kind` (`given`, `surname`, …).
fn name_part<'a>(card: &'a Map<String, Value>, kind: &str) -> Option<&'a str> {
    card.get("name")?
        .get("components")?
        .as_array()?
        .iter()
        .find(|c| c["kind"] == kind)?
        .get("value")?
        .as_str()
}

/// Свойства, по которым ищет `text`.
const TEXT_PROPS: [&str; 8] = [
    "name",
    "nicknames",
    "organizations",
    "emails",
    "phones",
    "onlineServices",
    "addresses",
    "notes",
];

/// Проверка фильтра (`FilterOperator` или `FilterCondition`, RFC 9610 §3.3.1).
fn matches(filter: &Value, card: &Map<String, Value>) -> MethodResult<bool> {
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
            results.push(matches(c, card)?);
        }
        return match op.as_str() {
            Some("AND") => Ok(results.iter().all(|r| *r)),
            Some("OR") => Ok(results.iter().any(|r| *r)),
            Some("NOT") => Ok(!results.iter().any(|r| *r)),
            _ => Err(MethodError::invalid("unknown filter operator")),
        };
    }
    for (key, val) in f {
        let unsupported = || MethodError::with("unsupportedFilter", key.clone());
        let ok = match key.as_str() {
            "inAddressBook" => {
                let id = val.as_str().ok_or_else(unsupported)?;
                card["addressBookIds"].get(id).is_some()
            }
            "hasMember" => {
                let uid = val.as_str().ok_or_else(unsupported)?;
                card.get("members").and_then(|m| m.get(uid)) == Some(&Value::Bool(true))
            }
            "uid" => card.get("uid") == Some(val),
            "kind" => {
                let kind = card.get("kind").and_then(Value::as_str);
                val.as_str().ok_or_else(unsupported)? == kind.unwrap_or("individual")
            }
            "name/given" | "name/surname" | "name/surname2" => {
                let needle = val.as_str().ok_or_else(unsupported)?.to_lowercase();
                name_part(card, &key[5..]).is_some_and(|v| v.to_lowercase().contains(&needle))
            }
            _ => {
                let props: &[&str] = match key.as_str() {
                    "text" => &TEXT_PROPS,
                    "name" => &["name"],
                    "nickname" => &["nicknames"],
                    "organization" => &["organizations"],
                    "email" => &["emails"],
                    "phone" => &["phones"],
                    "onlineService" => &["onlineServices"],
                    "address" => &["addresses"],
                    "note" => &["notes"],
                    _ => return Err(unsupported()),
                };
                let needle = val.as_str().ok_or_else(unsupported)?.to_lowercase();
                contains(card, props, &needle)
            }
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Свойства сортировки (RFC 9610 §3.3.2 и `uid`).
const SORT_PROPS: [&str; 6] = [
    "created",
    "updated",
    "uid",
    "name/given",
    "name/surname",
    "name/surname2",
];

fn sort_key(card: &Map<String, Value>, prop: &str) -> String {
    match prop.strip_prefix("name/") {
        Some(kind) => name_part(card, kind).unwrap_or_default().to_lowercase(),
        None => card
            .get(prop)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
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

/// Идентификаторы карточек по фильтру и сортировке.
async fn run_query(ctx: &Ctx<'_>, a: &QueryArgs) -> MethodResult<Vec<String>> {
    let sort = a.sort.as_deref().unwrap_or_default();
    if sort
        .iter()
        .any(|c| !SORT_PROPS.contains(&c.property.as_str()))
    {
        return Err(MethodError::new("unsupportedSort"));
    }
    let store = ctx.jmap.dav_store()?;
    let books = book_ids(store.as_ref(), ctx.scope).await?;
    let filter = a.filter.as_ref().filter(|f| !f.is_null());
    let mut cards = Vec::new();
    for obj in load(store.as_ref(), ctx.scope, &books).await? {
        let card = card_json(&obj);
        if match filter {
            Some(f) => matches(f, &card)?,
            None => true,
        } {
            cards.push(card);
        }
    }
    if cards.is_empty()
        && let Some(f) = filter
    {
        // Фильтр проверяется и без карточек.
        matches(f, &Map::new())?;
    }
    cards.sort_by(|x, y| {
        sort.iter()
            .map(|c| {
                let o = sort_key(x, &c.property).cmp(&sort_key(y, &c.property));
                if c.is_ascending { o } else { o.reverse() }
            })
            .find(|o| *o != Ordering::Equal)
            .unwrap_or_else(|| sort_key(x, "id").cmp(&sort_key(y, "id")))
    });
    Ok(cards.iter().map(|c| sort_key(c, "id")).collect())
}

/// `ContactCard/query`.
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
        "canCalculateChanges": true,
    }));
    apply_window(&ids, &a.window, ctx.jmap.cfg.max_query_limit, &mut out)?;
    Ok(out)
}

/// `ContactCard/queryChanges` по журналу объектов адресных книг.
pub(crate) async fn query_changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let qc: QueryChangesArgs = parse_args(args.clone())?;
    ctx.check_account(&qc.account_id)?;
    let mut q = args;
    q.retain(|k, _| matches!(k.as_str(), "accountId" | "filter" | "sort"));
    let a: QueryArgs = parse_args(q)?;
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

// ---------- Изменение ----------

/// Патч RFC 8620 §5.3: путь — указатель JSON без ведущего `/`, `null` удаляет свойство. Путь через массив или
/// несуществующее свойство — `invalidPatch`.
pub(crate) fn apply_patch(
    card: &mut Map<String, Value>,
    patch: &Map<String, Value>,
) -> Result<(), Value> {
    for (path, val) in patch {
        let parts: Vec<String> = path
            .split('/')
            .map(|p| p.replace("~1", "/").replace("~0", "~"))
            .collect();
        let Some((last, parents)) = parts.split_last() else {
            continue;
        };
        let mut cur = &mut *card;
        for p in parents {
            cur = match cur.get_mut(p) {
                Some(Value::Object(m)) => m,
                _ => return Err(set_error("invalidPatch", Some(path), &[])),
            };
        }
        if val.is_null() {
            cur.remove(last);
        } else {
            cur.insert(last.clone(), val.clone());
        }
    }
    Ok(())
}

/// Итог записи как ошибка `/set`; `Ok` — записано.
fn put_error(r: tp_contracts::Result<PutOutcome>) -> MethodResult<Result<DavObjectId, Value>> {
    Ok(match r {
        Ok(PutOutcome::Stored(p)) => Ok(p.id),
        Ok(PutOutcome::UidConflict { .. }) => Err(invalid(
            "uid is used by another card of the address book",
            &["uid"],
        )),
        Ok(PutOutcome::PreconditionFailed) => Err(set_error(
            "serverFail",
            Some("the card was changed concurrently"),
            &[],
        )),
        Err(Error::NotFound) => Err(invalid("address book not found", &["addressBookIds"])),
        Err(e) => Err(store_error(e)?),
    })
}

async fn create_one(ctx: &Ctx<'_>, v: &Value) -> MethodResult<Result<Value, Value>> {
    let store = ctx.jmap.dav_store()?;
    let Value::Object(src) = v else {
        return Ok(Err(set_error("invalidProperties", None, &[])));
    };
    let mut card = src.clone();
    if card.contains_key("id") {
        return Ok(Err(set_error("invalidProperties", None, &["id"])));
    }
    let book = match card.get("addressBookIds") {
        Some(v) => single_book(v).and_then(|id| CollectionId::from_str(ctx.resolve_id(id)).ok()),
        None => default_book(&store.list_collections(ctx.scope, KIND).await?),
    };
    let Some(book) = book else {
        let why = "exactly one existing address book is required";
        return Ok(Err(invalid(why, &["addressBookIds"])));
    };
    let now = utc_date(OffsetDateTime::now_utc());
    let object_id = DavObjectId::new();
    card.entry("@type").or_insert_with(|| json!("Card"));
    card.entry("version").or_insert_with(|| json!("1.0"));
    card.entry("uid")
        .or_insert_with(|| json!(format!("urn:uuid:{object_id}")));
    card.entry("created").or_insert_with(|| now.clone());
    card.insert("updated".into(), now.clone());
    let server_set = json!({ "uid": card["uid"], "created": card["created"], "updated": now });
    let (body, index) = match to_vcard(card) {
        Ok(x) => x,
        Err(e) => return Ok(Err(e)),
    };
    let name = format!("{object_id}.vcf");
    let put = store
        .put_object(
            ctx.scope,
            book,
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
    let Some(obj) = find(store.as_ref(), ctx.scope, id).await? else {
        return Ok(Err(set_error("notFound", None, &[])));
    };
    let Value::Object(patch) = patch else {
        return Ok(Err(set_error("invalidPatch", None, &[])));
    };
    let before = card_json(&obj);
    let mut card = before.clone();
    if let Err(e) = apply_patch(&mut card, patch) {
        return Ok(Err(e));
    }
    if card.get("id") != before.get("id") {
        return Ok(Err(set_error("invalidProperties", None, &["id"])));
    }
    if card.get("uid") != before.get("uid") {
        return Ok(Err(invalid("uid cannot be changed", &["uid"])));
    }
    let book = obj.collection.to_string();
    if card.get("addressBookIds").and_then(single_book) != Some(book.as_str()) {
        let why = "moving a card to another address book is not supported";
        return Ok(Err(invalid(why, &["addressBookIds"])));
    }
    let now = utc_date(OffsetDateTime::now_utc());
    card.insert("updated".into(), now.clone());
    let (body, index) = match to_vcard(card) {
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

/// `ContactCard/set`.
pub(crate) async fn set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: SetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
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
        let Some(obj) = find(store.as_ref(), ctx.scope, &rid).await? else {
            out.not_destroyed
                .insert(id, set_error("notFound", None, &[]));
            continue;
        };
        match store
            .delete_object(ctx.scope, obj.collection, &obj.name, Precondition::Any)
            .await
        {
            Ok(DeleteOutcome::Deleted(_)) => out.destroyed.push(id),
            Ok(DeleteOutcome::PreconditionFailed) | Err(Error::NotFound) => {
                out.not_destroyed
                    .insert(id, set_error("notFound", None, &[]));
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

/// `ContactCard/copy`: копирование между учётными записями; доступна только своя, поэтому источник —
/// `fromAccountNotFound` (общие адресные книги — JMAP-32).
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
    use std::sync::Arc;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use calcard::vcard::VCard;
    use serde_json::{Value, json};
    use tp_contracts::dav::DavStore;

    use crate::contacts::tests::{call, create_book};
    use crate::testenv::{Env, http10};

    /// Корпус vCard: полная карточка, организация, vCard 3.0 с группой и расширениями.
    const CORPUS: &[(&str, &str)] = &[
        (
            "ivan.vcf",
            "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:urn:uuid:11111111-2222-3333-4444-555555555555\r\n\
             FN:Иван Петров\r\nN:Петров;Иван;Сергеевич;;\r\nNICKNAME:Ваня\r\n\
             EMAIL;TYPE=work;PREF=1:ivan@example.org\r\nEMAIL;TYPE=home:vanya@example.com\r\n\
             TEL;TYPE=cell:+7 900 123-45-67\r\nORG:ООО Ромашка;Отдел продаж\r\nTITLE:Менеджер\r\n\
             ADR;TYPE=work:;;ул. Ленина\\, 1;Москва;;101000;Россия\r\nBDAY:19850412\r\n\
             NOTE:Заметка\\nвторая строка\r\nCATEGORIES:друзья,работа\r\nX-CUSTOM;X-P=1:значение\r\n\
             URL:https://example.org/ivan\r\nREV:20261101T100000Z\r\nEND:VCARD\r\n",
        ),
        (
            "romashka.vcf",
            "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:org-romashka\r\nKIND:org\r\nFN:ООО Ромашка\r\n\
             ORG:ООО Ромашка\r\nEMAIL:info@romashka.example\r\nTEL;TYPE=work,voice:+7 495 000-00-00\r\n\
             END:VCARD\r\n",
        ),
        (
            "anna.vcf",
            "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:anna-3\r\nFN:Anna Schmidt\r\nN:Schmidt;Anna;;;\r\n\
             item1.EMAIL;TYPE=INTERNET:anna@example.net\r\nitem1.X-ABLabel:Личная\r\n\
             TEL;TYPE=HOME:+49 30 1234567\r\nX-PHONETIC-FIRST-NAME:Anna\r\nEND:VCARD\r\n",
        ),
    ];

    /// Сервер CardDAV поверх того же хранилища и входа.
    pub(crate) async fn dav(env: &Env) -> std::net::SocketAddr {
        let app = tp_dav::router(
            tp_dav::Deps {
                auth: Arc::new(env.auth.clone()),
                store: Arc::new(env.dav.clone()),
            },
            tp_dav::DavConfig::default(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await });
        addr
    }

    async fn dav_request(
        addr: std::net::SocketAddr,
        method: &str,
        path: &str,
        body: &str,
    ) -> (u16, String) {
        let auth = STANDARD.encode("user@example.org:app-secret");
        let req = format!(
            "{method} {path} HTTP/1.0\r\nHost: dav.test\r\nAuthorization: Basic {auth}\r\n\
             Content-Type: text/vcard; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let (status, _, body) = http10(addr, req.as_bytes()).await;
        (status, String::from_utf8(body).expect("utf-8"))
    }

    /// Убирает `isOrdered`: calcard добавляет его компонентам имени и адреса после записи vCard из JSContact
    /// (параметр `JSCOMPS` фиксирует порядок) — данные карточки при этом те же.
    fn unordered(v: &mut Value) {
        match v {
            Value::Object(m) => {
                m.remove("isOrdered");
                m.values_mut().for_each(unordered);
            }
            Value::Array(a) => a.iter_mut().for_each(unordered),
            _ => {}
        }
    }

    /// Карточка для сравнения: без свойств JMAP, `updated` и `isOrdered`.
    fn plain(card: &Value) -> Value {
        let mut v = card.clone();
        unordered(&mut v);
        for k in ["id", "addressBookIds", "updated"] {
            v.as_object_mut().expect("card").remove(k);
        }
        v
    }

    /// JSContact из vCard (calcard) для сравнения.
    fn jscontact(vcard: &str) -> Value {
        let js = VCard::parse(vcard)
            .unwrap_or_else(|_| panic!("vcard: {vcard}"))
            .into_jscontact::<String, String>();
        plain(&serde_json::to_value(js).expect("json"))
    }

    async fn book_path(env: &Env, book: &str) -> String {
        let c = env
            .dav
            .get_collection(env.scope, book.parse().expect("id"))
            .await
            .expect("get")
            .expect("book");
        format!("/dav/addressbooks/user@example.org/{}/", c.name)
    }

    /// Карточки, записанные CardDAV, читаются JMAP без потери полей; изменение из JMAP сохраняет остальное.
    #[tokio::test]
    async fn carddav_to_jmap() {
        let env = Env::new().await;
        let acc = env.account();
        let addr = dav(&env).await;
        let book = create_book(&env, "Контакты").await;
        let base = book_path(&env, &book).await;
        for (name, body) in CORPUS {
            let (status, resp) = dav_request(addr, "PUT", &format!("{base}{name}"), body).await;
            assert_eq!(status, 201, "{name}: {resp}");
        }
        let got = call(&env, "ContactCard/get", json!({"accountId": acc})).await;
        let list = got["list"].as_array().expect("list");
        assert_eq!(list.len(), CORPUS.len(), "{got}");
        for (name, body) in CORPUS {
            let want = jscontact(body);
            let card = list
                .iter()
                .find(|c| c["uid"] == want["uid"])
                .unwrap_or_else(|| panic!("{name}: {got}"));
            assert_eq!(card["addressBookIds"], json!({&book: true}), "{name}");
            assert_eq!(plain(card), want, "{name}");
        }
        let ivan = list
            .iter()
            .find(|c| c["name"]["full"] == "Иван Петров")
            .expect("ivan");
        assert_eq!(
            ivan["emails"]["k1"]["address"], "ivan@example.org",
            "{ivan}"
        );
        assert_eq!(ivan["organizations"]["k1"]["name"], "ООО Ромашка", "{ivan}");

        // Изменение из JMAP: новая заметка и телефон; остальные поля vCard — как были.
        let id = ivan["id"].as_str().expect("id");
        let r = call(
            &env,
            "ContactCard/set",
            json!({"accountId": acc, "update": {id: {
                "notes/k1/note": "Новая заметка",
                "phones/p2": {"number": "+7 495 111-22-33"},
                "nicknames": null,
            }}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        let (status, body) = dav_request(addr, "GET", &format!("{base}ivan.vcf"), "").await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("X-CUSTOM;X-P=1:значение"), "{body}");
        let mut want = jscontact(CORPUS[0].1);
        want["notes"]["k1"]["note"] = json!("Новая заметка");
        want["phones"]["p2"] = json!({"number": "+7 495 111-22-33"});
        want.as_object_mut().expect("card").remove("nicknames");
        assert_eq!(jscontact(&body), want, "{body}");
    }

    /// Карточка, созданная JMAP, читается CardDAV; обратно — те же поля.
    #[tokio::test]
    async fn jmap_to_carddav() {
        let env = Env::new().await;
        let acc = env.account();
        let addr = dav(&env).await;
        let book = create_book(&env, "Контакты").await;
        let base = book_path(&env, &book).await;
        let card = json!({
            "@type": "Card", "version": "1.0",
            "addressBookIds": {&book: true},
            "name": {"full": "Мария Сидорова", "components": [
                {"kind": "surname", "value": "Сидорова"}, {"kind": "given", "value": "Мария"}]},
            "emails": {"work": {"address": "maria@example.org", "contexts": {"work": true}, "pref": 1}},
            "phones": {"m": {"number": "+7 911 000-11-22", "features": {"mobile": true}}},
            "organizations": {"o": {"name": "АО Вектор", "units": [{"name": "Бухгалтерия"}]}},
            "titles": {"t": {"name": "Главный бухгалтер", "kind": "title"}},
            "addresses": {"a": {"contexts": {"work": true}, "components": [
                {"kind": "name", "value": "Невский пр., 10"}, {"kind": "locality", "value": "Санкт-Петербург"},
                {"kind": "postcode", "value": "191186"}, {"kind": "country", "value": "Россия"}]}},
            "notes": {"n": {"note": "Звонить после 10"}},
            "keywords": {"клиенты": true},
            "anniversaries": {"b": {"kind": "birth",
                "date": {"@type": "PartialDate", "year": 1990, "month": 3, "day": 8}}},
        });
        let r = call(
            &env,
            "ContactCard/set",
            json!({"accountId": acc, "create": {"c": card}}),
        )
        .await;
        assert!(r["notCreated"].is_null(), "{r}");
        let id = r["created"]["c"]["id"].as_str().expect("id").to_owned();
        let uid = r["created"]["c"]["uid"].as_str().expect("uid").to_owned();
        assert!(uid.starts_with("urn:uuid:"), "{r}");

        let name = env
            .dav
            .objects_by_id(env.scope, &[id.parse().expect("id")])
            .await
            .expect("object")[0]
            .name
            .clone();
        let (status, body) = dav_request(addr, "GET", &format!("{base}{name}"), "").await;
        assert_eq!(status, 200, "{body}");
        for line in [
            "FN:Мария Сидорова",
            "maria@example.org",
            "+7 911 000-11-22",
            "АО Вектор;Бухгалтерия",
            "Главный бухгалтер",
            "Санкт-Петербург",
            "NOTE",
            "CATEGORIES:клиенты",
            "BDAY",
        ] {
            assert!(body.replace("\r\n ", "").contains(line), "{line}: {body}");
        }
        assert_eq!(
            VCard::parse(&body).expect("vcard").uid(),
            Some(uid.as_str())
        );

        // Обратно в JMAP — все заданные поля на месте.
        let got = call(
            &env,
            "ContactCard/get",
            json!({"accountId": acc, "ids": [id, "nope"]}),
        )
        .await;
        assert_eq!(got["notFound"], json!(["nope"]));
        assert_eq!(got["list"][0]["addressBookIds"], json!({&book: true}));
        let back = plain(&got["list"][0]);
        for (key, want) in plain(&card).as_object().expect("card") {
            assert_eq!(&back[key], want, "{key}: {back}");
        }
        assert_eq!(back, jscontact(&body));

        // Замена vCard через CardDAV видна JMAP под тем же id.
        let replaced = body.replace("FN:Мария Сидорова", "FN:Мария Иванова");
        let (status, resp) = dav_request(addr, "PUT", &format!("{base}{name}"), &replaced).await;
        assert_eq!(status, 204, "{resp}");
        let got = call(
            &env,
            "ContactCard/get",
            json!({"accountId": acc, "ids": [id],
            "properties": ["name"]}),
        )
        .await;
        assert_eq!(got["list"][0]["name"]["full"], "Мария Иванова", "{got}");
        assert!(got["list"][0].get("emails").is_none(), "{got}");
    }

    async fn create_card(env: &Env, book: &str, full: &str, given: &str, email: &str) -> String {
        let r = call(
            env,
            "ContactCard/set",
            json!({"accountId": env.account(), "create": {"c": {
                "addressBookIds": {book: true},
                "name": {"full": full, "components": [{"kind": "given", "value": given}]},
                "emails": {"e": {"address": email}},
                "phones": {"p": {"number": "+7 900 555-00-00"}},
            }}}),
        )
        .await;
        r["created"]["c"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{r}"))
            .to_owned()
    }

    #[tokio::test]
    async fn query_by_name_and_email() {
        let env = Env::new().await;
        let acc = env.account();
        let work = create_book(&env, "Работа").await;
        let home = create_book(&env, "Дом").await;
        let ivan = create_card(&env, &work, "Иван Петров", "Иван", "ivan@example.org").await;
        let anna = create_card(&env, &work, "Анна Смирнова", "Анна", "anna@corp.example").await;
        let boris = create_card(&env, &home, "Борис Иванов", "Борис", "boris@example.org").await;
        let q = |filter: Value| {
            let (env, acc) = (&env, acc.clone());
            async move {
                call(
                    env,
                    "ContactCard/query",
                    json!({"accountId": acc, "filter": filter,
                    "sort": [{"property": "name/given"}]}),
                )
                .await
            }
        };
        assert_eq!(
            q(json!({"name": "иван"})).await["ids"],
            json!([boris, ivan])
        );
        assert_eq!(q(json!({"name/given": "иван"})).await["ids"], json!([ivan]));
        assert_eq!(
            q(json!({"email": "EXAMPLE.ORG"})).await["ids"],
            json!([boris, ivan])
        );
        assert_eq!(
            q(json!({"email": "anna@corp.example"})).await["ids"],
            json!([anna])
        );
        assert_eq!(
            q(json!({"inAddressBook": work})).await["ids"],
            json!([anna, ivan])
        );
        assert_eq!(
            q(json!({"phone": "555"})).await["ids"],
            json!([anna, boris, ivan])
        );
        assert_eq!(q(json!({"text": "corp"})).await["ids"], json!([anna]));
        assert_eq!(
            q(json!({"operator": "AND", "conditions": [
                {"inAddressBook": work}, {"operator": "NOT", "conditions": [{"name": "анна"}]}]}))
            .await["ids"],
            json!([ivan])
        );
        let uid = call(
            &env,
            "ContactCard/get",
            json!({"accountId": acc, "ids": [anna]}),
        )
        .await["list"][0]["uid"]
            .clone();
        assert_eq!(q(json!({"uid": uid})).await["ids"], json!([anna]));
        assert_eq!(
            q(json!({"createdBefore": "2026-01-01T00:00:00Z"})).await["type"],
            "unsupportedFilter"
        );

        let r = call(
            &env,
            "ContactCard/query",
            json!({"accountId": acc, "sort": [{"property": "name/given", "isAscending": false}],
                   "limit": 2, "calculateTotal": true}),
        )
        .await;
        assert_eq!(r["ids"], json!([ivan, boris]), "{r}");
        assert_eq!(r["total"], 3);
        assert_eq!(r["canCalculateChanges"], true);
        let r = call(
            &env,
            "ContactCard/query",
            json!({"accountId": acc, "sort": [{"property": "nope"}]}),
        )
        .await;
        assert_eq!(r["type"], "unsupportedSort", "{r}");
    }

    #[tokio::test]
    async fn changes_set_and_copy() {
        let env = Env::new().await;
        let acc = env.account();
        let book = create_book(&env, "Работа").await;
        let other = create_book(&env, "Дом").await;
        let state0 = call(
            &env,
            "ContactCard/get",
            json!({"accountId": acc, "ids": []}),
        )
        .await["state"]
            .clone();
        let ivan = create_card(&env, &book, "Иван Петров", "Иван", "ivan@example.org").await;
        let anna = create_card(&env, &book, "Анна Смирнова", "Анна", "anna@example.org").await;
        let c = call(
            &env,
            "ContactCard/changes",
            json!({"accountId": acc, "sinceState": state0}),
        )
        .await;
        assert_eq!(c["created"].as_array().map(Vec::len), Some(2), "{c}");
        let state1 = c["newState"].clone();

        // Без книги — книга по умолчанию; ошибки создания и изменения.
        let r = call(
            &env,
            "ContactCard/set",
            json!({"accountId": acc,
                "create": {
                    "d": {"name": {"full": "Без книги"}},
                    "two": {"addressBookIds": {&book: true, &other: true}, "name": {"full": "X"}},
                    "dup": {"addressBookIds": {&book: true}, "uid": "same", "name": {"full": "A"}},
                    "dup2": {"addressBookIds": {&book: true}, "uid": "same", "name": {"full": "B"}},
                    "withid": {"id": "x", "name": {"full": "C"}},
                },
                "update": {
                    &ivan: {"name/full": "Иван Петрович Петров"},
                    &anna: {"addressBookIds": {&other: true}},
                    "nope": {"name/full": "x"},
                },
                "destroy": ["nope"]}),
        )
        .await;
        assert!(r["created"]["d"]["id"].is_string(), "{r}");
        assert_eq!(
            r["notCreated"]["two"]["properties"],
            json!(["addressBookIds"]),
            "{r}"
        );
        assert!(r["created"]["dup"]["id"].is_string(), "{r}");
        assert_eq!(r["notCreated"]["dup2"]["properties"], json!(["uid"]), "{r}");
        assert_eq!(
            r["notCreated"]["withid"]["type"], "invalidProperties",
            "{r}"
        );
        assert!(
            r["updated"]
                .as_object()
                .is_some_and(|u| u.contains_key(&ivan)),
            "{r}"
        );
        assert_eq!(
            r["notUpdated"][&anna]["properties"],
            json!(["addressBookIds"]),
            "{r}"
        );
        assert_eq!(r["notUpdated"]["nope"]["type"], "notFound", "{r}");
        assert_eq!(r["notDestroyed"]["nope"]["type"], "notFound", "{r}");

        let c = call(
            &env,
            "ContactCard/changes",
            json!({"accountId": acc, "sinceState": state1}),
        )
        .await;
        assert_eq!(c["updated"], json!([ivan]), "{c}");
        assert_eq!(c["created"].as_array().map(Vec::len), Some(2), "{c}");
        let qc = call(
            &env,
            "ContactCard/queryChanges",
            json!({"accountId": acc, "sinceQueryState": state1, "filter": {"name": "петр"}}),
        )
        .await;
        assert_eq!(qc["removed"], json!([ivan]), "{qc}");
        assert_eq!(qc["added"], json!([{"id": ivan, "index": 0}]), "{qc}");

        let r = call(
            &env,
            "ContactCard/set",
            json!({"accountId": acc, "destroy": [anna]}),
        )
        .await;
        assert_eq!(r["destroyed"], json!([anna]), "{r}");
        let g = call(
            &env,
            "ContactCard/get",
            json!({"accountId": acc, "ids": [anna]}),
        )
        .await;
        assert_eq!(g["notFound"], json!([anna]));

        let r = call(
            &env,
            "ContactCard/copy",
            json!({"fromAccountId": "other", "accountId": acc, "create": {}}),
        )
        .await;
        assert_eq!(r["type"], "fromAccountNotFound", "{r}");
        let r = call(
            &env,
            "ContactCard/copy",
            json!({"fromAccountId": acc, "accountId": acc, "create": {}}),
        )
        .await;
        assert_eq!(r["type"], "invalidArguments", "{r}");
    }
}
