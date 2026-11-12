//! Личные списки отправителей: `SenderList/get`, `SenderList/set` (capability `urn:thepost:senderlists`; JMAP-17,
//! SRS 4.1, решение 2026-11-04-exchange-parity).
//!
//! Объект один на учётную запись (`id` = `singleton`, как VacationResponse в RFC 8621 §8), создать или удалить его
//! нельзя (`singleton`). Свойства — списки адресов (`user@example.org`) и доменов (`example.org`):
//! - `safe` — надёжные отправители: их письма при доставке попадают во «Входящие» поверх вердикта антиспама;
//! - `blocked` — заблокированные: их письма попадают в «Спам».
//!
//! Списки хранит и применяет при доставке [`SenderLists`](tp_contracts::senders::SenderLists)
//! ([`Jmap::with_sender_lists`](crate::Jmap::with_sender_lists)); `SenderList/set` заменяет список целиком, записи
//! приводятся к хранимому виду (нижний регистр, без повторов). `state` — состояние списков хранилища.

use serde_json::{Map, Value, json};
use tp_contracts::Error;
use tp_contracts::senders::{MAX_SENDER_ENTRIES, MAX_SENDER_ENTRY_LEN, SenderList};

use crate::core::Ctx;
use crate::core::error::{MethodError, MethodResult, set_error};
use crate::core::request::{GetArgs, SetArgs, SetOutcome, object, parse_args, select};

/// Идентификатор единственного объекта.
const SINGLETON: &str = "singleton";

/// Значение capability учётной записи: пределы списков.
pub(crate) fn account_capability() -> Value {
    json!({ "maxEntries": MAX_SENDER_ENTRIES, "maxEntryLength": MAX_SENDER_ENTRY_LEN })
}

fn list_object(list: &SenderList) -> Map<String, Value> {
    object(json!({ "id": SINGLETON, "safe": list.safe, "blocked": list.blocked }))
}

async fn state(ctx: &Ctx<'_>) -> MethodResult<String> {
    let store = ctx.jmap.sender_lists_store()?;
    Ok(store.sender_list_state(ctx.scope).await?.to_string())
}

/// `SenderList/get`.
pub(crate) async fn get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let store = ctx.jmap.sender_lists_store()?;
    let state = state(ctx).await?;
    let me = list_object(&store.sender_list(ctx.scope).await?);
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    match &a.ids {
        None => list.push(me),
        Some(ids) => {
            for i in ids {
                if i == SINGLETON {
                    list.push(me.clone());
                } else {
                    not_found.push(i.clone());
                }
            }
        }
    }
    let list: Vec<Value> = list
        .into_iter()
        .map(|o| Value::Object(select(o, a.properties.as_deref())))
        .collect();
    Ok(object(json!({
        "accountId": a.account_id,
        "state": state,
        "list": list,
        "notFound": not_found,
    })))
}

/// Применяет патч к спискам: свойства `safe` и `blocked` — массивы строк, заменяют список целиком.
fn apply_patch(mut list: SenderList, patch: &Value) -> Result<SenderList, Value> {
    let Value::Object(p) = patch else {
        return Err(set_error("invalidPatch", None, &[]));
    };
    for (k, v) in p {
        let entries: Option<Vec<String>> = v
            .as_array()
            .and_then(|a| a.iter().map(|e| e.as_str().map(str::to_owned)).collect());
        match (k.as_str(), entries) {
            ("safe", Some(e)) => list.safe = e,
            ("blocked", Some(e)) => list.blocked = e,
            _ => return Err(set_error("invalidProperties", None, &[k.as_str()])),
        }
    }
    Ok(list)
}

/// `SenderList/set`: только изменение объекта `singleton`.
pub(crate) async fn set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: SetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    a.check_limit(ctx.jmap.cfg.max_objects_in_set)?;
    let store = ctx.jmap.sender_lists_store()?;
    let old = state(ctx).await?;
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }
    let mut out = SetOutcome::default();
    for (cid, _) in a.create.unwrap_or_default() {
        out.not_created
            .insert(cid, set_error("singleton", None, &[]));
    }
    for id in a.destroy.unwrap_or_default() {
        out.not_destroyed
            .insert(id, set_error("singleton", None, &[]));
    }
    for (id, patch) in a.update.unwrap_or_default() {
        if id != SINGLETON {
            out.not_updated.insert(id, set_error("notFound", None, &[]));
            continue;
        }
        let current = store.sender_list(ctx.scope).await?;
        let wanted = match apply_patch(current, &patch) {
            Ok(l) => l,
            Err(e) => {
                out.not_updated.insert(id, e);
                continue;
            }
        };
        match store.put_sender_list(ctx.scope, wanted.clone()).await {
            Ok(_) => {
                // Сервер привёл записи к хранимому виду — возвращает изменившиеся свойства.
                let stored = store.sender_list(ctx.scope).await?;
                let changed = if stored == wanted {
                    Value::Null
                } else {
                    json!({ "safe": stored.safe, "blocked": stored.blocked })
                };
                out.updated.insert(id, changed);
            }
            Err(Error::Invalid(why)) => {
                out.not_updated
                    .insert(id, set_error("invalidProperties", Some(&why), &[]));
            }
            Err(e) => return Err(e.into()),
        }
    }
    let new = state(ctx).await?;
    Ok(out.response(&a.account_id, &old, &new))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tp_contracts::senders::{SenderLists, SenderVerdict};

    use crate::testenv::Env;

    const URI: &str = "urn:thepost:senderlists";

    async fn call(env: &Env, method: &str, args: Value) -> Value {
        let resp = env
            .request(json!({
                "using": ["urn:ietf:params:jmap:core", URI],
                "methodCalls": [[method, args, "c0"]],
            }))
            .await;
        resp["methodResponses"][0][1].clone()
    }

    /// Пустые списки; замена списков приводит записи к хранимому виду, меняет состояние и действует при доставке
    /// (вердикт хранилища).
    #[tokio::test]
    async fn get_and_set() {
        let env = Env::new().await;
        let acc = env.account();
        let session = env.jmap.session(&env.principal, "https://mail.example.org");
        assert!(session["capabilities"][URI].is_object(), "{session}");
        assert_eq!(session["primaryAccounts"][URI], acc);
        assert_eq!(
            session["accounts"][&acc]["accountCapabilities"][URI]["maxEntries"],
            1024
        );

        let g = call(
            &env,
            "SenderList/get",
            json!({"accountId": acc, "ids": null}),
        )
        .await;
        assert_eq!(
            g["list"],
            json!([{"id": "singleton", "safe": [], "blocked": []}]),
            "{g}"
        );
        let s = call(
            &env,
            "SenderList/set",
            json!({"accountId": acc, "ifInState": g["state"], "update": {"singleton": {
                "safe": ["Boss@Example.org", "@partner.test", "boss@example.org"],
                "blocked": ["spam.test"],
            }}}),
        )
        .await;
        assert_eq!(s["oldState"], g["state"], "{s}");
        assert_ne!(s["newState"], s["oldState"]);
        assert_eq!(
            s["updated"]["singleton"],
            json!({"safe": ["boss@example.org", "partner.test"], "blocked": ["spam.test"]})
        );
        let g = call(
            &env,
            "SenderList/get",
            json!({"accountId": acc, "ids": ["singleton", "x"], "properties": ["blocked"]}),
        )
        .await;
        assert_eq!(
            g["list"],
            json!([{"id": "singleton", "blocked": ["spam.test"]}])
        );
        assert_eq!(g["notFound"], json!(["x"]));
        assert_eq!(g["state"], s["newState"]);
        let verdict = |from: &'static str| {
            let lists = env.sender_lists.clone();
            let scope = env.scope;
            async move { lists.sender_verdict(scope, from).await.expect("verdict") }
        };
        assert_eq!(
            verdict("anyone@partner.test").await,
            Some(SenderVerdict::Safe)
        );
        assert_eq!(verdict("x@spam.test").await, Some(SenderVerdict::Blocked));

        // Патч одного свойства не трогает другое.
        let s = call(
            &env,
            "SenderList/set",
            json!({"accountId": acc, "update": {"singleton": {"blocked": []}}}),
        )
        .await;
        assert_eq!(s["updated"]["singleton"], Value::Null, "{s}");
        let g = call(&env, "SenderList/get", json!({"accountId": acc})).await;
        assert_eq!(
            g["list"][0]["safe"],
            json!(["boss@example.org", "partner.test"])
        );
        assert_eq!(g["list"][0]["blocked"], json!([]));
    }

    /// Ошибки: создание и удаление — `singleton`, чужой id — `notFound`, неизвестное свойство и недопустимая запись —
    /// `invalidProperties`, чужое состояние — `stateMismatch`; без хранилища capability нет.
    #[tokio::test]
    async fn set_errors() {
        let env = Env::new().await;
        let acc = env.account();
        let s = call(
            &env,
            "SenderList/set",
            json!({"accountId": acc,
                "create": {"n": {"safe": []}},
                "destroy": ["singleton"],
                "update": {"other": {"safe": []}}}),
        )
        .await;
        assert_eq!(s["notCreated"]["n"]["type"], "singleton", "{s}");
        assert_eq!(s["notDestroyed"]["singleton"]["type"], "singleton");
        assert_eq!(s["notUpdated"]["other"]["type"], "notFound");
        for patch in [
            json!({"trusted": []}),
            json!({"safe": "boss@example.org"}),
            json!({"safe": ["два слова"]}),
            json!({"blocked": ["@"]}),
        ] {
            let s = call(
                &env,
                "SenderList/set",
                json!({"accountId": acc, "update": {"singleton": patch}}),
            )
            .await;
            assert_eq!(
                s["notUpdated"]["singleton"]["type"], "invalidProperties",
                "{s}"
            );
            assert_eq!(s["newState"], s["oldState"]);
        }
        let s = call(
            &env,
            "SenderList/set",
            json!({"accountId": acc, "ifInState": "чужое", "update": {"singleton": {"safe": []}}}),
        )
        .await;
        assert_eq!(s["type"], "stateMismatch");

        let bare = Env::bare().await;
        let session = bare
            .jmap
            .session(&bare.principal, "https://mail.example.org");
        assert!(session["capabilities"][URI].is_null());
        let resp = bare
            .request(json!({"using": ["urn:ietf:params:jmap:core", URI],
                "methodCalls": [["SenderList/get", {"accountId": bare.account()}, "c0"]]}))
            .await;
        assert_eq!(
            resp["type"], "urn:ietf:params:jmap:error:unknownCapability",
            "{resp}"
        );
    }
}
