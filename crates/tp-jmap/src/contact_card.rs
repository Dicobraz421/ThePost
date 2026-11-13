//! JMAP Contacts — карточки (RFC 9610 §3; JMAP-26): `ContactCard/get`, `/changes`, `/query`, `/queryChanges`,
//! `/set`, `/copy` поверх [`DavStore`].
//!
//! Хранится vCard — тот же объект, что читает и пишет CardDAV; JSContact ↔ vCard — calcard, индекс объекта —
//! `tp_dav::index::card`. Карточка — в одной адресной книге (`maxAddressBooksPerCard` = 1); смена `addressBookIds`
//! переносит её в другую книгу с прежним `id` ([`DavStore::move_object`], JMAP-34). `/copy` — между учётными
//! записями, а доступна только своя: чужая — `fromAccountNotFound`. Фильтры запроса — RFC 9610 §3.3.1 (`text` — имя,
//! псевдонимы, организации, адреса, телефоны, сервисы, почтовые адреса, заметки). Условие `name`, `name/given`,
//! `name/surname` или `email` (само или внутри `AND`) берёт кандидатов из поискового индекса
//! ([`DavStore::find_cards`]: полное имя, части имени, псевдонимы, адреса e-mail) и проверяется по карточке; прочие
//! фильтры, в том числе `text` (ищет и по полям вне индекса), — перебором карточек книг.

use std::cmp::Ordering;
use std::str::FromStr;

use calcard::jscontact::JSContact;
use calcard::vcard::VCard;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tp_contracts::Error;
use tp_contracts::dav::{
    CollectionId, DavObject, DavObjectId, DavStore, DeleteOutcome, MoveOutcome, ObjectIndex,
    Precondition, PutOutcome,
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
                card.get("addressBookIds").and_then(|b| b.get(id)).is_some()
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

/// Условия, кандидатов которых даёт поисковый индекс карточки: его строки (полное имя, части имени, псевдонимы,
/// адреса e-mail) содержат всё, в чём ищут эти условия.
const INDEXED: [&str; 4] = ["name", "name/given", "name/surname", "email"];

/// Строка для выборки кандидатов по индексу: самое длинное значение условий [`INDEXED`] фильтра — самого
/// условия или одного из условий `AND` (на любом уровне); иначе — `None` (нужен перебор всех карточек).
fn index_needle(filter: &Value) -> Option<String> {
    let f = filter.as_object()?;
    let found = match f.get("operator") {
        Some(op) if op == "AND" => f
            .get("conditions")?
            .as_array()?
            .iter()
            .filter_map(index_needle)
            .max_by_key(String::len),
        Some(_) => None,
        None => INDEXED
            .iter()
            .filter_map(|k| f.get(*k)?.as_str())
            .map(|v| v.trim().to_lowercase())
            .max_by_key(String::len),
    };
    found.filter(|n| !n.is_empty())
}

/// Кандидаты запроса по индексу ([`DavStore::find_cards`]) из книг `books`. Выборка книги, обрезанная пределом
/// (больше объектов, чем допускает книга, — лимиты хранилища изменились), заменяется всеми карточками книги.
async fn candidates(
    store: &dyn DavStore,
    scope: AccountScope,
    books: &[CollectionId],
    needle: &str,
) -> MethodResult<Vec<DavObject>> {
    let limit = store.limits().max_objects.saturating_add(1);
    let mut out = Vec::new();
    for b in books {
        match store.find_cards(scope, *b, needle, limit).await {
            Ok(found) if found.len() < limit => out.extend(found),
            Ok(_) => out.extend(load(store, scope, std::slice::from_ref(b)).await?),
            // Книгу удалили между запросами.
            Err(Error::NotFound) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(out)
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
    let objects = match filter.and_then(index_needle) {
        Some(needle) => candidates(store.as_ref(), ctx.scope, &books, &needle).await?,
        None => load(store.as_ref(), ctx.scope, &books).await?,
    };
    let mut cards = Vec::new();
    for obj in objects {
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
    let Some(to) = card
        .get("addressBookIds")
        .and_then(single_book)
        .and_then(|id| CollectionId::from_str(ctx.resolve_id(id)).ok())
    else {
        let why = "exactly one existing address book is required";
        return Ok(Err(invalid(why, &["addressBookIds"])));
    };
    let now = utc_date(OffsetDateTime::now_utc());
    card.insert("updated".into(), now.clone());
    let (body, index) = match to_vcard(card) {
        Ok(x) => x,
        Err(e) => return Ok(Err(e)),
    };
    let mut etag = obj.etag.clone();
    if to != obj.collection {
        // Перенос в другую книгу (JMAP-34): тот же объект и `id`, затем запись изменённой карточки.
        let pre = Precondition::IfMatch(obj.etag.clone());
        match store
            .move_object(ctx.scope, obj.collection, &obj.name, to, pre)
            .await
        {
            Ok(MoveOutcome::Moved(m)) => etag = m.etag,
            Ok(MoveOutcome::UidConflict { .. }) => {
                let why = "uid is used by another card of the address book";
                return Ok(Err(invalid(why, &["uid"])));
            }
            Ok(MoveOutcome::PreconditionFailed) => {
                let why = "the card was changed concurrently or its resource name is taken in the address book";
                return Ok(Err(set_error("serverFail", Some(why), &[])));
            }
            Err(Error::NotFound | Error::Invalid(_)) => {
                let why = "exactly one existing address book is required";
                return Ok(Err(invalid(why, &["addressBookIds"])));
            }
            Err(e) => return Ok(Err(store_error(e)?)),
        }
    }
    let pre = Precondition::IfMatch(etag);
    let put = store
        .put_object(ctx.scope, to, &obj.name, body, index, pre)
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

    /// Корпус vCard `tp-testkit` (KIT-09); карточке без `UID` (выгрузки клиентов) — `UID` по имени файла: CardDAV
    /// его требует (RFC 6352 §5.1).
    async fn corpus() -> Vec<(String, String)> {
        let cards = tokio::task::spawn_blocking(tp_testkit::vcards)
            .await
            .expect("join")
            .expect("corpus");
        assert!(!cards.is_empty());
        cards
            .into_iter()
            .map(|c| {
                let text = if c.text.lines().any(|l| l.starts_with("UID")) {
                    c.text
                } else {
                    let uid = format!("BEGIN:VCARD\r\nUID:corpus-{}\r\n", c.name);
                    c.text.replacen("BEGIN:VCARD\r\n", &uid, 1)
                };
                (format!("{}.vcf", c.name), text)
            })
            .collect()
    }

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

    /// Параметр `PREF` без значения (vCard 2.1, свойство вне JSContact): calcard читает его как `"0"`, а пишет
    /// `PREF=`, которое читает как `""`. Значения нет в обоих случаях; для сравнения — `""`.
    fn bare_pref(mut v: Value) -> Value {
        if let Some(props) = v
            .pointer_mut("/vCard/properties")
            .and_then(Value::as_array_mut)
        {
            for p in props {
                if p[1]["pref"] == "0" {
                    p[1]["pref"] = json!("");
                }
            }
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

    /// Корпус vCard туда и обратно: карточки, записанные CardDAV, читаются JMAP без потери полей; запись из JMAP
    /// (vCard заново из JSContact) сохраняет все поля, изменение — только то, что изменено.
    #[tokio::test]
    async fn carddav_to_jmap() {
        let env = Env::new().await;
        let acc = env.account();
        let addr = dav(&env).await;
        let book = create_book(&env, "Контакты").await;
        let base = book_path(&env, &book).await;
        let corpus = corpus().await;
        for (name, body) in &corpus {
            if body.contains("\r\nVERSION:2.1\r\n") {
                // CardDAV принимает vCard 3.0 и 4.0 (`supported-address-data`); выгрузку 2.1 кладёт импорт —
                // прямо в хранилище.
                let index = tp_dav::index::card(body).expect("index");
                let put = env
                    .dav
                    .put_object(
                        env.scope,
                        book.parse().expect("id"),
                        name,
                        body.clone(),
                        index,
                        tp_contracts::dav::Precondition::Any,
                    )
                    .await;
                assert!(put.is_ok(), "{name}: {put:?}");
                continue;
            }
            let (status, resp) = dav_request(addr, "PUT", &format!("{base}{name}"), body).await;
            assert_eq!(status, 201, "{name}: {resp}");
        }
        let got = call(&env, "ContactCard/get", json!({"accountId": acc})).await;
        let list = got["list"].as_array().expect("list");
        assert_eq!(list.len(), corpus.len(), "{got}");
        let mut lost: Vec<&str> = Vec::new();
        for (name, body) in &corpus {
            let want = jscontact(body);
            let card = list
                .iter()
                .find(|c| c["uid"] == want["uid"])
                .unwrap_or_else(|| panic!("{name}: {got}"));
            assert_eq!(card["addressBookIds"], json!({&book: true}), "{name}");
            assert_eq!(plain(card), want, "{name}");

            // Обратно: JMAP пишет vCard заново из JSContact — поля те же.
            let id = card["id"].as_str().expect("id");
            let r = call(
                &env,
                "ContactCard/set",
                json!({"accountId": acc, "update": {id: {}}}),
            )
            .await;
            assert!(r["notUpdated"].is_null(), "{name}: {r}");
            let (status, back) = dav_request(addr, "GET", &format!("{base}{name}"), "").await;
            assert_eq!(status, 200, "{name}: {back}");
            let (back_js, want) = (bare_pref(jscontact(&back)), bare_pref(want));
            if back_js != want {
                eprintln!("{name}: {back}\n  было: {want}\n  стало: {back_js}");
                lost.push(name);
            }
        }
        // Известная потеря calcard при записи vCard из JSContact: `TZ;VALUE=TEXT` (выгрузка Thunderbird)
        // записывается пустым, пояс адреса теряется.
        assert_eq!(lost, ["12-export-thunderbird.vcf"], "потери полей");

        // Изменение из JMAP: новая заметка и телефон; остальные поля vCard, в том числе X-свойства, — как были.
        let (name, body) = &corpus[0];
        let want = jscontact(body);
        let card = list.iter().find(|c| c["uid"] == want["uid"]).expect("card");
        let id = card["id"].as_str().expect("id");
        let r = call(
            &env,
            "ContactCard/set",
            json!({"accountId": acc, "update": {id: {
                "notes": {"n": {"note": "Новая заметка"}},
                "phones/p2": {"number": "+7 495 111-22-33"},
                "nicknames": null,
            }}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        let (status, back) = dav_request(addr, "GET", &format!("{base}{name}"), "").await;
        assert_eq!(status, 200, "{back}");
        assert!(back.contains("X-THEPOST-TEST:"), "{back}");
        let mut want = want;
        want["notes"] = json!({"n": {"note": "Новая заметка"}});
        want["phones"]["p2"] = json!({"number": "+7 495 111-22-33"});
        want.as_object_mut().expect("card").remove("nicknames");
        assert_eq!(jscontact(&back), want, "{back}");
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
                    &anna: {"addressBookIds": {&book: true, &other: true}},
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

    /// Смена `addressBookIds` переносит карточку в другую книгу (JMAP-34): прежний `id`, изменение — в
    /// `ContactCard/changes`, ресурс — в другой книге CardDAV.
    #[tokio::test]
    async fn move_between_books() {
        let env = Env::new().await;
        let acc = env.account();
        let addr = dav(&env).await;
        let work = create_book(&env, "Работа").await;
        let home = create_book(&env, "Дом").await;
        let ivan = create_card(&env, &work, "Иван Петров", "Иван", "ivan@example.org").await;
        let state0 = call(
            &env,
            "ContactCard/get",
            json!({"accountId": acc, "ids": []}),
        )
        .await["state"]
            .clone();
        let name = env
            .dav
            .objects_by_id(env.scope, &[ivan.parse().expect("id")])
            .await
            .expect("object")[0]
            .name
            .clone();

        let r = call(
            &env,
            "ContactCard/set",
            json!({"accountId": acc, "update": {&ivan: {
                "addressBookIds": {&home: true}, "name/full": "Иван Сергеевич Петров"}}}),
        )
        .await;
        assert!(
            r["updated"]
                .as_object()
                .is_some_and(|u| u.contains_key(&ivan)),
            "{r}"
        );
        let g = call(
            &env,
            "ContactCard/get",
            json!({"accountId": acc, "ids": [&ivan]}),
        )
        .await;
        assert_eq!(g["list"][0]["id"], json!(ivan), "{g}");
        assert_eq!(g["list"][0]["addressBookIds"], json!({&home: true}), "{g}");
        assert_eq!(g["list"][0]["name"]["full"], "Иван Сергеевич Петров", "{g}");
        let c = call(
            &env,
            "ContactCard/changes",
            json!({"accountId": acc, "sinceState": state0}),
        )
        .await;
        assert_eq!(c["updated"], json!([ivan]), "{c}");
        assert_eq!(c["created"], json!([]), "{c}");
        assert_eq!(c["destroyed"], json!([]), "{c}");
        let q = call(
            &env,
            "ContactCard/query",
            json!({"accountId": acc, "filter": {"inAddressBook": home}}),
        )
        .await;
        assert_eq!(q["ids"], json!([ivan]), "{q}");

        // CardDAV: ресурс с прежним именем — в новой книге, в старой его нет.
        let (status, body) = dav_request(
            addr,
            "GET",
            &format!("{}{name}", book_path(&env, &home).await),
            "",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("FN:Иван Сергеевич Петров"), "{body}");
        let (status, _) = dav_request(
            addr,
            "GET",
            &format!("{}{name}", book_path(&env, &work).await),
            "",
        )
        .await;
        assert_eq!(status, 404);

        // Несколько книг или несуществующая книга — отказ, карточка на месте.
        let r = call(
            &env,
            "ContactCard/set",
            json!({"accountId": acc, "update": {
                &ivan: {"addressBookIds": {&home: true, &work: true}}}}),
        )
        .await;
        assert_eq!(
            r["notUpdated"][&ivan]["properties"],
            json!(["addressBookIds"]),
            "{r}"
        );
        let nope = tp_contracts::dav::CollectionId::new().to_string();
        let r = call(
            &env,
            "ContactCard/set",
            json!({"accountId": acc, "update": {&ivan: {"addressBookIds": {&nope: true}}}}),
        )
        .await;
        assert_eq!(
            r["notUpdated"][&ivan]["properties"],
            json!(["addressBookIds"]),
            "{r}"
        );
        let g = call(
            &env,
            "ContactCard/get",
            json!({"accountId": acc, "ids": [&ivan], "properties": ["addressBookIds"]}),
        )
        .await;
        assert_eq!(g["list"][0]["addressBookIds"], json!({&home: true}), "{g}");
    }

    /// Запрос по имени и адресу берёт кандидатов из поискового индекса, а не читает все карточки: объект без
    /// поисковых строк в индексе находит только перебор (`text`).
    #[tokio::test]
    async fn query_uses_index() {
        use tp_contracts::dav::{Component, ObjectIndex, Precondition, PutOutcome};
        let env = Env::new().await;
        let acc = env.account();
        let book = create_book(&env, "Контакты").await;
        let ivan = create_card(&env, &book, "Иван Петров", "Иван", "ivan@example.org").await;
        let body = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:hidden\r\nFN:Иван Скрытый\r\nN:Скрытый;Иван;;;\r\n\
                    EMAIL:hidden@example.org\r\nEND:VCARD\r\n";
        let put = env
            .dav
            .put_object(
                env.scope,
                book.parse().expect("id"),
                "hidden.vcf",
                body.to_owned(),
                ObjectIndex::new(Component::Card, "hidden", None),
                Precondition::Any,
            )
            .await
            .expect("put");
        let PutOutcome::Stored(hidden) = put else {
            panic!("put: not stored");
        };
        let hidden = hidden.id.to_string();
        let q = |filter: Value| {
            let (env, acc) = (&env, acc.clone());
            async move {
                call(
                    env,
                    "ContactCard/query",
                    json!({"accountId": acc, "filter": filter}),
                )
                .await["ids"]
                    .clone()
            }
        };
        assert_eq!(q(json!({"name": "иван"})).await, json!([ivan]));
        assert_eq!(q(json!({"name/given": "иван"})).await, json!([ivan]));
        assert_eq!(q(json!({"email": "example.org"})).await, json!([ivan]));
        assert_eq!(q(json!({"email": "hidden"})).await, json!([]));
        assert_eq!(
            q(json!({"operator": "AND", "conditions": [
                {"inAddressBook": book}, {"name/surname": "скрыт"}]}))
            .await,
            json!([])
        );
        // Перебор: `text`, `OR` — и объект вне индекса.
        assert_eq!(q(json!({"text": "hidden"})).await, json!([hidden]));
        assert_eq!(
            q(json!({"operator": "OR", "conditions": [{"email": "hidden"}]})).await,
            json!([hidden])
        );
    }
}
