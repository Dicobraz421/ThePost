//! JMAP Contacts — адресные книги (RFC 9610 §2; JMAP-25): `AddressBook/get`, `/changes`, `/set` поверх
//! [`DavStore`] (вид [`CollectionKind::AddressBook`]) — те же коллекции, что у CardDAV.
//!
//! `name` — отображаемое имя коллекции, сегмент URL новой книги — её `id`. `myRights` — права владельца; общий
//! доступ (`shareWith`) — JMAP-32. Книга по умолчанию (`isDefault`) — назначенная (`onSuccessSetIsDefault`,
//! [`Collection::is_default`]), иначе правило [`default_of`]; `isSubscribed` — [`Collection::subscribed`] (JMAP-36).
//! Состояние — [`DavStore::state`] вида (общее для `AddressBook` и `ContactCard`); push — по `DavChanged`.

use std::str::FromStr;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tp_contracts::Error;
use tp_contracts::dav::{Collection, CollectionId, CollectionKind, DavStore, default_of};
use tp_contracts::meta::AccountScope;

use crate::core::Ctx;
use crate::core::error::{MethodError, MethodResult, changes_error, set_error};
use crate::core::request::{
    ChangesArgs, GetArgs, SetArgs, SetOutcome, changes_response, map_changes, object, parse_args,
    parse_state, select, state_string,
};

pub(crate) const KIND: CollectionKind = CollectionKind::AddressBook;

/// Сегмент URL книги по умолчанию.
const DEFAULT_NAME: &str = tp_contracts::dav::DEFAULT_COLLECTION_NAME;

/// Значение capability учётной записи: карточка — в одной книге (объект CardDAV).
pub(crate) fn account_capability() -> Value {
    json!({ "maxAddressBooksPerCard": 1, "mayCreateAddressBook": true })
}

/// Книга по умолчанию среди `books` — правило контракта [`default_of`]: назначенная, иначе прежнее правило.
pub(crate) fn default_book(books: &[Collection]) -> Option<CollectionId> {
    default_of(books).map(|b| b.id)
}

/// Текущее состояние контактов.
pub(crate) async fn state(store: &dyn DavStore, scope: AccountScope) -> MethodResult<String> {
    Ok(state_string(store.state(scope, KIND).await?))
}

fn rights() -> Value {
    json!({ "mayRead": true, "mayWrite": true, "mayShare": false, "mayDelete": true })
}

fn book_object(b: &Collection, default: Option<CollectionId>) -> Map<String, Value> {
    object(json!({
        "id": b.id.to_string(),
        "name": b.display_name,
        "description": b.description,
        "sortOrder": b.order,
        "isDefault": default == Some(b.id),
        "isSubscribed": b.subscribed,
        "shareWith": null,
        "myRights": rights(),
    }))
}

/// `AddressBook/get`.
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
    let default = default_book(&books);
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    let props = a.properties.as_deref();
    match &a.ids {
        None => {
            list.extend(books.iter().map(|b| select(book_object(b, default), props)));
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
                    Some(b) => list.push(select(book_object(b, default), props)),
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

/// `AddressBook/changes` по журналу коллекций (изменения карточек — `ContactCard/changes`).
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
struct BookSetArgs {
    #[serde(default)]
    on_destroy_remove_contents: bool,
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
            ("isSubscribed", Value::Bool(b)) => {
                c.subscribed = *b;
                true
            }
            ("shareWith", Value::Null) => true,
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

/// Ошибка хранилища как `SetError`; прочие — ошибка метода.
pub(crate) fn store_error(e: Error) -> MethodResult<Value> {
    Ok(match e {
        Error::QuotaExceeded => set_error("overQuota", None, &[]),
        Error::NotFound => set_error("notFound", None, &[]),
        Error::Invalid(m) => set_error("invalidProperties", Some(&m), &[]),
        e => return Err(e.into()),
    })
}

/// `onSuccessSetIsDefault` (RFC 9610 §2.3, draft-ietf-jmap-calendars): после `/set` без ошибок назначает коллекцию
/// `want` вида `kind` (`#<creation id>` — созданную этим вызовом) коллекцией по умолчанию; признак прежней снимает
/// хранилище ([`DavStore::update_collection`]). Сменившийся `isDefault` обеих — в `created` или `updated` ответа.
/// Нет такой коллекции — назначение пропускается без ошибки (RFC 9610 §2.3).
pub(crate) async fn set_default(
    ctx: &Ctx<'_>,
    store: &dyn DavStore,
    kind: CollectionKind,
    want: Option<&str>,
    out: &mut SetOutcome,
) -> MethodResult<()> {
    if !out.not_created.is_empty() || !out.not_updated.is_empty() || !out.not_destroyed.is_empty() {
        return Ok(());
    }
    let Some(id) = want.and_then(|w| CollectionId::from_str(ctx.resolve_id(w)).ok()) else {
        return Ok(());
    };
    let all = store.list_collections(ctx.scope, kind).await?;
    let old = default_of(&all).map(|c| c.id);
    let Some(mut c) = all.into_iter().find(|c| c.id == id) else {
        return Ok(());
    };
    if !c.is_default {
        c.is_default = true;
        match store.update_collection(ctx.scope, c).await {
            Ok(_) => {}
            // Коллекцию удалили между запросами.
            Err(Error::NotFound) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    if old != Some(id) {
        report_default(out, id, true);
        if let Some(old) = old {
            report_default(out, old, false);
        }
    }
    Ok(())
}

/// Изменённый сервером `isDefault` коллекции `id` в ответе `/set`: в `created`, если она создана этим вызовом,
/// иначе в `updated`.
fn report_default(out: &mut SetOutcome, id: CollectionId, is_default: bool) {
    let id = id.to_string();
    let target = match out.created.values_mut().find(|v| v["id"] == id.as_str()) {
        Some(v) => v,
        None => out.updated.entry(id).or_insert(Value::Null),
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Value::Object(m) = target {
        m.insert("isDefault".into(), Value::Bool(is_default));
    }
}

/// `AddressBook/set`: создание, изменение (имя, описание, порядок, подписка), удаление (`onDestroyRemoveContents`),
/// книга по умолчанию (`onSuccessSetIsDefault`).
pub(crate) async fn set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let mut rest = args.clone();
    rest.retain(|k, _| k.starts_with("on"));
    let extra: BookSetArgs = parse_args(rest)?;
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
    let mut out = SetOutcome::default();
    for (cid, v) in a.create.unwrap_or_default() {
        let mut c = Collection::new(KIND, DEFAULT_NAME);
        c.name = c.id.to_string();
        if let Err(e) = apply_props(&mut c, &v, true) {
            out.not_created.insert(cid, e);
            continue;
        }
        let (id, subscribed) = (c.id, c.subscribed);
        match store.create_collection(ctx.scope, c).await {
            Ok(_) => {
                ctx.created.insert(cid.clone(), id.to_string());
                let books = store.list_collections(ctx.scope, KIND).await?;
                out.created.insert(
                    cid,
                    json!({
                        "id": id.to_string(),
                        "isDefault": default_book(&books) == Some(id),
                        "isSubscribed": subscribed,
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
        if !extra.on_destroy_remove_contents
            && !store.list_objects(ctx.scope, c.id).await?.is_empty()
        {
            out.not_destroyed
                .insert(id, set_error("addressBookHasContents", None, &[]));
            continue;
        }
        match store.delete_collection(ctx.scope, c.id).await {
            Ok(_) => out.destroyed.push(id),
            Err(e) => {
                out.not_destroyed.insert(id, store_error(e)?);
            }
        }
    }
    let want = extra.on_success_set_is_default.as_deref();
    set_default(ctx, store.as_ref(), KIND, want, &mut out).await?;
    let new = state(store.as_ref(), ctx.scope).await?;
    Ok(out.response(&a.account_id, &old, &new))
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::{Value, json};
    use tp_contracts::dav::{CollectionKind, DavStore, ObjectIndex, Precondition};

    use crate::testenv::Env;

    pub(crate) const USING: &[&str] =
        &["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"];

    pub(crate) async fn call(env: &Env, method: &str, args: Value) -> Value {
        let resp = env
            .request(json!({"using": USING, "methodCalls": [[method, args, "c0"]]}))
            .await;
        let name = &resp["methodResponses"][0][0];
        assert!(name == method || name == "error", "{resp}");
        resp["methodResponses"][0][1].clone()
    }

    /// Идентификаторы объектов ответа `/get` с `isDefault`.
    pub(crate) fn defaults(g: &Value) -> Vec<String> {
        g["list"]
            .as_array()
            .expect("list")
            .iter()
            .filter(|x| x["isDefault"] == true)
            .filter_map(|x| x["id"].as_str().map(str::to_owned))
            .collect()
    }

    pub(crate) async fn create_book(env: &Env, name: &str) -> String {
        let r = call(
            env,
            "AddressBook/set",
            json!({"accountId": env.account(), "create": {"b": {"name": name}}}),
        )
        .await;
        r["created"]["b"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{r}"))
            .to_owned()
    }

    async fn put_card(env: &Env, book: &str, uid: &str) {
        let body = format!("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:{uid}\r\nFN:X\r\nEND:VCARD\r\n");
        let index = ObjectIndex::new(tp_contracts::dav::Component::Card, uid, None);
        env.dav
            .put_object(
                env.scope,
                book.parse().expect("id"),
                &format!("{uid}.vcf"),
                body,
                index,
                Precondition::Any,
            )
            .await
            .expect("put");
    }

    #[tokio::test]
    async fn session_lists_capability() {
        let env = Env::new().await;
        let s = env.jmap.session(&env.principal, "https://mail.example.org");
        let acc = env.account();
        assert!(s["capabilities"][USING[1]].is_object(), "{s}");
        assert_eq!(
            s["accounts"][&acc]["accountCapabilities"][USING[1]],
            json!({"maxAddressBooksPerCard": 1, "mayCreateAddressBook": true})
        );
        assert_eq!(s["primaryAccounts"][USING[1]], acc);
    }

    #[tokio::test]
    async fn create_rename_changes() {
        let env = Env::new().await;
        let acc = env.account();
        let r = call(&env, "AddressBook/get", json!({"accountId": acc})).await;
        assert_eq!(r["list"], json!([]), "{r}");
        let state0 = r["state"].as_str().expect("state").to_owned();

        let r = call(
            &env,
            "AddressBook/set",
            json!({"accountId": acc, "ifInState": state0, "create": {
                "b": {"name": "Личные", "description": "Мои контакты", "sortOrder": 3},
                "bad": {"description": "без имени", "color": "red"}}}),
        )
        .await;
        let id = r["created"]["b"]["id"].as_str().expect("id").to_owned();
        assert_eq!(r["created"]["b"]["isDefault"], true, "{r}");
        assert_eq!(r["notCreated"]["bad"]["type"], "invalidProperties", "{r}");
        assert_eq!(
            r["notCreated"]["bad"]["properties"],
            json!(["color", "name"])
        );
        let state1 = r["newState"].as_str().expect("state").to_owned();
        assert_ne!(state0, state1);

        // Та же коллекция видна CardDAV (общее хранилище).
        let books = env
            .dav
            .list_collections(env.scope, CollectionKind::AddressBook)
            .await
            .expect("list");
        assert_eq!(books[0].display_name, "Личные");
        assert_eq!(books[0].name, id);

        let r = call(
            &env,
            "AddressBook/set",
            json!({"accountId": acc, "update": {
                &id: {"name": "Работа", "description": null},
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
            "AddressBook/get",
            json!({"accountId": acc, "ids": [id, "zz"]}),
        )
        .await;
        assert_eq!(g["notFound"], json!(["zz"]));
        assert_eq!(
            g["list"][0],
            json!({
                "id": id, "name": "Работа", "description": null, "sortOrder": 3,
                "isDefault": true, "isSubscribed": true, "shareWith": null,
                "myRights": {"mayRead": true, "mayWrite": true, "mayShare": false, "mayDelete": true},
            })
        );

        let c = call(
            &env,
            "AddressBook/changes",
            json!({"accountId": acc, "sinceState": state0}),
        )
        .await;
        assert_eq!(c["created"], json!([id]), "{c}");
        let c = call(
            &env,
            "AddressBook/changes",
            json!({"accountId": acc, "sinceState": state1}),
        )
        .await;
        assert_eq!(c["updated"], json!([id]), "{c}");
        assert_eq!(c["created"], json!([]));
        assert_eq!(c["hasMoreChanges"], false);
        assert_eq!(c["newState"], g["state"]);
        let c = call(
            &env,
            "AddressBook/changes",
            json!({"accountId": acc, "sinceState": "nope"}),
        )
        .await;
        assert_eq!(c["type"], "cannotCalculateChanges", "{c}");
    }

    #[tokio::test]
    async fn destroy_with_and_without_contents() {
        let env = Env::new().await;
        let acc = env.account();
        let empty = create_book(&env, "Пустая").await;
        let full = create_book(&env, "С карточками").await;
        put_card(&env, &full, "u1").await;
        let st = call(
            &env,
            "AddressBook/get",
            json!({"accountId": acc, "ids": []}),
        )
        .await["state"]
            .clone();

        let r = call(
            &env,
            "AddressBook/set",
            json!({"accountId": acc, "destroy": [empty, full, "nope"]}),
        )
        .await;
        assert_eq!(r["destroyed"], json!([empty]), "{r}");
        assert_eq!(
            r["notDestroyed"][&full]["type"], "addressBookHasContents",
            "{r}"
        );
        assert_eq!(r["notDestroyed"]["nope"]["type"], "notFound", "{r}");

        let r = call(
            &env,
            "AddressBook/set",
            json!({"accountId": acc, "destroy": [full], "onDestroyRemoveContents": true}),
        )
        .await;
        assert_eq!(r["destroyed"], json!([full]), "{r}");
        let c = call(
            &env,
            "AddressBook/changes",
            json!({"accountId": acc, "sinceState": st}),
        )
        .await;
        let mut destroyed: Vec<&str> = c["destroyed"]
            .as_array()
            .expect("destroyed")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        destroyed.sort_unstable();
        let mut want = [empty.as_str(), full.as_str()];
        want.sort_unstable();
        assert_eq!(destroyed, want, "{c}");
    }

    /// Смена книги по умолчанию (`onSuccessSetIsDefault`) и подписка (JMAP-36).
    #[tokio::test]
    async fn default_book_and_subscription() {
        let env = Env::new().await;
        let acc = env.account();
        let a = create_book(&env, "A").await;
        let b = create_book(&env, "B").await;
        let g = call(&env, "AddressBook/get", json!({"accountId": acc})).await;
        let before = defaults(&g);
        assert_eq!(before.len(), 1, "{g}");
        let other = if before[0] == a { &b } else { &a };

        // Та же книга — без изменений в ответе.
        let r = call(
            &env,
            "AddressBook/set",
            json!({"accountId": acc, "onSuccessSetIsDefault": before[0]}),
        )
        .await;
        assert!(r["updated"].is_null(), "{r}");

        let r = call(
            &env,
            "AddressBook/set",
            json!({"accountId": acc, "update": {other: {"isSubscribed": false}},
                   "onSuccessSetIsDefault": other}),
        )
        .await;
        assert_eq!(r["updated"][other], json!({"isDefault": true}), "{r}");
        assert_eq!(r["updated"][&before[0]], json!({"isDefault": false}), "{r}");
        let g = call(&env, "AddressBook/get", json!({"accountId": acc})).await;
        assert_eq!(defaults(&g), [other.clone()], "{g}");
        let got = g["list"]
            .as_array()
            .expect("list")
            .iter()
            .find(|x| x["id"] == other.as_str())
            .expect("book");
        assert_eq!(got["isSubscribed"], false, "{g}");

        // Новая книга по умолчанию в том же вызове: признак — в `created`.
        let r = call(
            &env,
            "AddressBook/set",
            json!({"accountId": acc, "create": {"n": {"name": "C", "isSubscribed": false}},
                   "onSuccessSetIsDefault": "#n"}),
        )
        .await;
        assert_eq!(r["created"]["n"]["isDefault"], true, "{r}");
        assert_eq!(r["created"]["n"]["isSubscribed"], false, "{r}");
        assert_eq!(r["updated"][other], json!({"isDefault": false}), "{r}");
        let c = r["created"]["n"]["id"].as_str().expect("id").to_owned();
        let g = call(&env, "AddressBook/get", json!({"accountId": acc})).await;
        assert_eq!(defaults(&g), [c.clone()], "{g}");

        // Неизвестная книга или ошибка в вызове — признак не меняется, ошибки метода нет.
        let r = call(
            &env,
            "AddressBook/set",
            json!({"accountId": acc, "onSuccessSetIsDefault": "nope"}),
        )
        .await;
        assert!(r["newState"].is_string(), "{r}");
        let r = call(
            &env,
            "AddressBook/set",
            json!({"accountId": acc, "update": {"nope": {"name": "x"}},
                   "onSuccessSetIsDefault": a}),
        )
        .await;
        assert_eq!(r["notUpdated"]["nope"]["type"], "notFound", "{r}");
        let g = call(&env, "AddressBook/get", json!({"accountId": acc})).await;
        assert_eq!(defaults(&g), [c], "{g}");
    }

    /// Изменение адресной книги → push `AddressBook`, `ContactCard`.
    #[tokio::test]
    async fn push_types() {
        use futures::StreamExt;
        use tp_contracts::bus::EventBus;
        let env = Env::new().await;
        let mut events = env.bus.subscribe(env.scope).await.expect("subscribe");
        create_book(&env, "A").await;
        let ev = events.next().await.expect("event");
        assert_eq!(
            crate::push::changed_types(&ev.kind),
            ["AddressBook", "ContactCard"],
            "{ev:?}"
        );
    }
}
