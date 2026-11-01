//! Настройки пользователя: `UserSettings/get`, `UserSettings/set` (capability `urn:thepost:params:jmap:settings`).
//!
//! Объект один на учётную запись (`id` = `singleton`, как VacationResponse в RFC 8621 §8), создать или удалить
//! его нельзя (`singleton`). Свойства (решение 2026-11-01-trash-purge):
//! - `trashPurge` — выбор пользователя для «очистить корзину»: `"hide"`, `"erase"` или `null` (действует значение
//!   администратора). Задать выбор можно, только если администратор разрешил (`user_can_choose`), иначе
//!   `forbidden`; сбросить (`null`) можно всегда;
//! - `trashPurgeEffective` — действующее значение (только чтение);
//! - `trashPurgeUserCanChoose` — администратор разрешил выбор (только чтение).

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tp_contracts::Error;
use tp_contracts::retention::{TrashPurge, TrashPurgeSetting};

use crate::core::error::{MethodError, MethodResult, set_error};
use crate::core::request::{GetArgs, object, parse_args, select};
use crate::core::{Ctx, digest};

/// Идентификатор единственного объекта.
const SINGLETON: &str = "singleton";

fn mode(m: TrashPurge) -> Value {
    json!(match m {
        TrashPurge::Hide => "hide",
        TrashPurge::Erase => "erase",
    })
}

fn settings_object(s: &TrashPurgeSetting) -> Map<String, Value> {
    object(json!({
        "id": SINGLETON,
        "trashPurge": s.user.map_or(Value::Null, mode),
        "trashPurgeEffective": mode(s.effective()),
        "trashPurgeUserCanChoose": s.rule.user_can_choose,
    }))
}

async fn current(ctx: &Ctx<'_>) -> MethodResult<Map<String, Value>> {
    let s = ctx.jmap.d.policies.trash_purge(ctx.scope).await?;
    Ok(settings_object(&s))
}

fn state(obj: &Map<String, Value>) -> String {
    digest(Value::Object(obj.clone()).to_string().as_bytes())
}

fn or_null(m: Map<String, Value>) -> Value {
    if m.is_empty() {
        Value::Null
    } else {
        Value::Object(m)
    }
}

/// `UserSettings/get`.
pub(crate) async fn get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let me = current(ctx).await?;
    let st = state(&me);
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
        "state": st,
        "list": list,
        "notFound": not_found,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetArgs {
    account_id: String,
    if_in_state: Option<String>,
    create: Option<Map<String, Value>>,
    update: Option<Map<String, Value>>,
    destroy: Option<Vec<String>>,
}

/// Новый выбор `trashPurge` из патча; ошибка — объект `SetError`.
fn parse_patch(patch: &Value) -> Result<Option<TrashPurge>, Value> {
    let Value::Object(p) = patch else {
        return Err(set_error("invalidPatch", None, &[]));
    };
    let bad: Vec<&str> = p
        .keys()
        .filter(|k| *k != "trashPurge")
        .map(String::as_str)
        .collect();
    if !bad.is_empty() {
        return Err(set_error("invalidProperties", None, &bad));
    }
    match p.get("trashPurge") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s == "hide" => Ok(Some(TrashPurge::Hide)),
        Some(Value::String(s)) if s == "erase" => Ok(Some(TrashPurge::Erase)),
        Some(_) => Err(set_error("invalidProperties", None, &["trashPurge"])),
    }
}

/// `UserSettings/set`: только изменение объекта `singleton`.
pub(crate) async fn set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: SetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let old = state(&current(ctx).await?);
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }
    let mut not_created = Map::new();
    for (cid, _) in a.create.unwrap_or_default() {
        not_created.insert(cid, set_error("singleton", None, &[]));
    }
    let mut not_destroyed = Map::new();
    for id in a.destroy.unwrap_or_default() {
        not_destroyed.insert(id, set_error("singleton", None, &[]));
    }
    let (mut updated, mut not_updated) = (Map::new(), Map::new());
    for (id, patch) in a.update.unwrap_or_default() {
        if id != SINGLETON {
            not_updated.insert(id, set_error("notFound", None, &[]));
            continue;
        }
        let choice = match parse_patch(&patch) {
            Ok(c) => c,
            Err(e) => {
                not_updated.insert(id, e);
                continue;
            }
        };
        match ctx
            .jmap
            .d
            .policies
            .set_user_trash_purge(ctx.scope, choice)
            .await
        {
            Ok(()) => {
                updated.insert(id, Value::Null);
            }
            Err(Error::Forbidden) => {
                let why = "trash.purge is set by the administrator";
                not_updated.insert(id, set_error("forbidden", Some(why), &["trashPurge"]));
            }
            Err(e) => return Err(e.into()),
        }
    }
    let new = state(&current(ctx).await?);
    Ok(object(json!({
        "accountId": a.account_id,
        "oldState": old,
        "newState": new,
        "created": null,
        "updated": or_null(updated),
        "destroyed": null,
        "notCreated": or_null(not_created),
        "notUpdated": or_null(not_updated),
        "notDestroyed": or_null(not_destroyed),
    })))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tp_contracts::retention::{TrashPurge, TrashPurgeRule};
    use tp_testkit::PolicyLevel;

    use crate::testenv::Env;

    const USING: &[&str] = &[
        "urn:ietf:params:jmap:core",
        "urn:thepost:params:jmap:settings",
    ];

    /// Ответ метода; ошибка метода — её объект (`type`).
    async fn call(env: &Env, method: &str, args: serde_json::Value) -> serde_json::Value {
        let resp = env
            .request(json!({"using": USING, "methodCalls": [[method, args, "c0"]]}))
            .await;
        let name = &resp["methodResponses"][0][0];
        assert!(name == method || name == "error", "{resp}");
        resp["methodResponses"][0][1].clone()
    }

    fn allow_choice(env: &Env, allow: bool) {
        let rule = TrashPurgeRule {
            mode: TrashPurge::Hide,
            user_can_choose: allow,
        };
        env.policies
            .set_trash_purge_rule(env.scope.tenant, PolicyLevel::Tenant, Some(rule));
    }

    #[tokio::test]
    async fn session_lists_settings() {
        let env = Env::new().await;
        let s = env.jmap.session(&env.principal, "https://mail.example.org");
        let acc = env.account();
        assert!(s["capabilities"]["urn:thepost:params:jmap:settings"].is_object());
        assert_eq!(
            s["primaryAccounts"]["urn:thepost:params:jmap:settings"],
            acc
        );
    }

    #[tokio::test]
    async fn get_defaults_to_admin_hide() {
        let env = Env::new().await;
        let acc = env.account();
        let r = call(&env, "UserSettings/get", json!({"accountId": acc})).await;
        assert_eq!(
            r["list"],
            json!([{
                "id": "singleton",
                "trashPurge": null,
                "trashPurgeEffective": "hide",
                "trashPurgeUserCanChoose": false,
            }])
        );
        let r = call(
            &env,
            "UserSettings/get",
            json!({"accountId": acc, "ids": ["x"], "properties": ["trashPurge"]}),
        )
        .await;
        assert_eq!(r["notFound"], json!(["x"]));
    }

    #[tokio::test]
    async fn user_chooses_only_when_allowed() {
        let env = Env::new().await;
        let acc = env.account();
        let set = |v: serde_json::Value| json!({"accountId": acc, "update": {"singleton": {"trashPurge": v}}});
        // Администратор не разрешил выбор — forbidden, действует его значение.
        let r = call(&env, "UserSettings/set", set(json!("erase"))).await;
        assert_eq!(r["notUpdated"]["singleton"]["type"], "forbidden", "{r}");
        assert_eq!(r["oldState"], r["newState"]);
        // Разрешил — выбор пользователя действует.
        allow_choice(&env, true);
        let r = call(&env, "UserSettings/set", set(json!("erase"))).await;
        assert_eq!(r["updated"], json!({"singleton": null}), "{r}");
        assert_ne!(r["oldState"], r["newState"]);
        let r = call(&env, "UserSettings/get", json!({"accountId": acc})).await;
        assert_eq!(r["list"][0]["trashPurge"], "erase");
        assert_eq!(r["list"][0]["trashPurgeEffective"], "erase");
        // Запретил снова — значение администратора приоритетнее, выбор хранится, но не действует.
        allow_choice(&env, false);
        let r = call(&env, "UserSettings/get", json!({"accountId": acc})).await;
        assert_eq!(r["list"][0]["trashPurgeEffective"], "hide");
        // Сбросить можно всегда.
        let r = call(&env, "UserSettings/set", set(json!(null))).await;
        assert_eq!(r["updated"], json!({"singleton": null}), "{r}");
        let r = call(&env, "UserSettings/get", json!({"accountId": acc})).await;
        assert_eq!(r["list"][0]["trashPurge"], json!(null));
    }

    #[tokio::test]
    async fn set_rejects_bad_input() {
        let env = Env::new().await;
        allow_choice(&env, true);
        let acc = env.account();
        let r = call(
            &env,
            "UserSettings/set",
            json!({
                "accountId": acc,
                "create": {"n": {}},
                "destroy": ["singleton"],
                "update": {
                    "singleton": {"trashPurgeEffective": "erase"},
                    "other": {"trashPurge": "hide"},
                },
            }),
        )
        .await;
        assert_eq!(r["notCreated"]["n"]["type"], "singleton");
        assert_eq!(r["notDestroyed"]["singleton"]["type"], "singleton");
        assert_eq!(
            r["notUpdated"]["singleton"],
            json!({"type": "invalidProperties", "properties": ["trashPurgeEffective"]})
        );
        assert_eq!(r["notUpdated"]["other"]["type"], "notFound");
        let r = call(
            &env,
            "UserSettings/set",
            json!({"accountId": acc, "update": {"singleton": {"trashPurge": "wipe"}}}),
        )
        .await;
        assert_eq!(
            r["notUpdated"]["singleton"]["properties"],
            json!(["trashPurge"])
        );
        let r = call(
            &env,
            "UserSettings/set",
            json!({"accountId": acc, "ifInState": "nope", "update": {}}),
        )
        .await;
        assert_eq!(r["type"], "stateMismatch", "{r}");
    }
}
