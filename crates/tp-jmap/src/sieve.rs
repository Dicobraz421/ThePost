//! JMAP Sieve (RFC 9661; JMAP-24): `SieveScript/get`, `/set`, `/query`, `/validate` поверх
//! [`SieveStore`] и проверки `tp_sieve::compile` с пределами доставки ([`Jmap::with_sieve_limits`], JMAP-35).
//!
//! Скрипт в хранилище адресуется именем, поэтому `id` — отпечаток имени: переименование меняет `id` (новый — в
//! `updated`). `blobId` текста скрипта — `S<id>_<отпечаток текста>`, скачивается обычным `downloadUrl`. Текст для
//! `/set` и `/validate` — blob (загрузка `uploadUrl`); ошибки проверки — `invalidSieve` с позициями в `description`.
//! Состояние — [`SieveStore::state`] (общее с автоответом); `/changes` в RFC 9661 нет.

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tp_contracts::Error;
use tp_contracts::audit::{AuditAction, AuditObject};
use tp_contracts::meta::AccountScope;
use tp_contracts::sieve::{
    MAX_SCRIPT_NAME_LEN, ScriptInfo, SieveLimits, SieveStore, valid_script_name,
};

use crate::core::error::{MethodError, MethodResult, set_error};
use crate::core::request::{
    Comparator, GetArgs, QueryWindow, SetArgs, SetOutcome, apply_window, object, parse_args, select,
};
use crate::core::{Ctx, Jmap, digest};

/// Значение capability в сессии.
pub(crate) fn session_capability() -> Value {
    json!({ "implementation": "The Post" })
}

/// Значение capability учётной записи: лимиты хранилища, пределы разбора и выполнения `run` (те же, что при
/// доставке) и расширения интерпретатора.
pub(crate) fn account_capability(limits: SieveLimits, run: &tp_sieve::Limits) -> Value {
    json!({
        "maxSizeScriptName": MAX_SCRIPT_NAME_LEN * 4,
        "maxSizeScript": limits.max_script_size.min(run.max_script_size),
        "maxNumberScripts": limits.max_scripts,
        "maxNumberRedirects": run.max_redirects,
        "sieveExtensions": tp_sieve::EXTENSIONS,
        "notificationMethods": null,
        "externalLists": null,
    })
}

/// Идентификатор скрипта по имени.
fn script_id(name: &str) -> String {
    format!("s{}", digest(name.as_bytes()))
}

/// `blobId` текста скрипта.
fn script_blob_id(name: &str, text: &str) -> String {
    format!("S{}_{}", script_id(name), digest(text.as_bytes()))
}

impl Jmap {
    /// Текст скрипта по `blobId` (без префикса `S`); другой скрипт или уже другой текст — `None`.
    pub(crate) async fn script_blob(
        &self,
        scope: AccountScope,
        rest: &str,
    ) -> tp_contracts::Result<Option<String>> {
        let (Some(store), Some((id, _))) = (&self.sieve, rest.split_once('_')) else {
            return Ok(None);
        };
        for s in store.list(scope).await? {
            if script_id(&s.name) == id {
                let text = store.get(scope, &s.name).await?;
                return Ok(text.filter(|t| script_blob_id(&s.name, t)[1..] == *rest));
            }
        }
        Ok(None)
    }
}

async fn script_object(
    store: &dyn SieveStore,
    scope: AccountScope,
    s: &ScriptInfo,
) -> MethodResult<Map<String, Value>> {
    let text = store.get(scope, &s.name).await?.unwrap_or_default();
    Ok(object(json!({
        "id": script_id(&s.name),
        "name": s.name,
        "blobId": script_blob_id(&s.name, &text),
        "isActive": s.active,
    })))
}

/// `SieveScript/get`.
pub(crate) async fn get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let store = ctx.jmap.sieve_store()?;
    let state = store.state(ctx.scope).await?;
    let all = store.list(ctx.scope).await?;
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    let push = async |s: &ScriptInfo, list: &mut Vec<Value>| -> MethodResult<()> {
        let o = script_object(store.as_ref(), ctx.scope, s).await?;
        list.push(Value::Object(select(o, a.properties.as_deref())));
        Ok(())
    };
    match &a.ids {
        None => {
            for s in &all {
                push(s, &mut list).await?;
            }
        }
        Some(ids) => {
            let mut seen: Vec<&str> = Vec::new();
            for id in ids {
                let id = ctx.resolve_id(id);
                if seen.contains(&id) {
                    continue;
                }
                seen.push(id);
                match all.iter().find(|s| script_id(&s.name) == id) {
                    Some(s) => push(s, &mut list).await?,
                    None => not_found.push(id.to_owned()),
                }
            }
        }
    }
    Ok(object(json!({
        "accountId": a.account_id,
        "state": state.to_string(),
        "list": list,
        "notFound": not_found,
    })))
}

/// Ошибки проверки скрипта одной строкой: `line L, column C: описание`.
fn diagnostics(diags: &[tp_sieve::Diagnostic]) -> String {
    diags
        .iter()
        .map(|d| format!("line {}, column {}: {}", d.line, d.column, d.message))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Проверенный текст скрипта из blob (пределы — [`Jmap::with_sieve_limits`]); ошибка — объект `SetError`
/// (`blobNotFound`, `tooLarge` — больше лимита хранилища, `invalidSieve`).
async fn script_text(ctx: &Ctx<'_>, blob_id: &str) -> MethodResult<Result<String, Value>> {
    let store = ctx.jmap.sieve_store()?;
    let max = store.limits().max_script_size as u64;
    let data = match ctx.jmap.read_blob(ctx.scope, blob_id, max).await {
        Ok(Some(d)) => d,
        Ok(None) => return Ok(Err(set_error("blobNotFound", None, &["blobId"]))),
        Err(Error::Invalid(_)) => return Ok(Err(set_error("tooLarge", None, &["blobId"]))),
        Err(e) => return Err(e.into()),
    };
    let Ok(text) = String::from_utf8(data) else {
        let why = "line 1, column 1: script is not valid UTF-8";
        return Ok(Err(set_error("invalidSieve", Some(why), &[])));
    };
    Ok(
        match tp_sieve::compile(text.as_bytes(), &ctx.jmap.sieve_limits) {
            Ok(_) => Ok(text),
            Err(diags) => Err(set_error("invalidSieve", Some(&diagnostics(&diags)), &[])),
        },
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivateArgs {
    on_success_activate_script: Option<String>,
    #[serde(default)]
    on_success_deactivate_script: bool,
}

/// Свойства `/set`: имя (`Some(None)` — `null`) и blob; ошибка — `invalidProperties`.
fn script_props(v: &Value) -> Result<(Option<Option<String>>, Option<String>), Value> {
    let Value::Object(p) = v else {
        return Err(set_error("invalidProperties", None, &[]));
    };
    let (mut name, mut blob) = (None, None);
    let mut bad: Vec<&str> = Vec::new();
    for (key, val) in p {
        match (key.as_str(), val) {
            ("name", Value::Null) => name = Some(None),
            ("name", Value::String(s)) if valid_script_name(s) => name = Some(Some(s.clone())),
            ("blobId", Value::String(s)) => blob = Some(s.clone()),
            _ => bad.push(key),
        }
    }
    if bad.is_empty() {
        Ok((name, blob))
    } else {
        Err(set_error("invalidProperties", None, &bad))
    }
}

/// Ошибка хранилища как `SetError`; прочие — ошибка метода.
/// Аудит ящика: изменены серверные правила (скрипт `what`, автоответ).
pub(crate) async fn rules_changed(ctx: &Ctx<'_>, what: &str) {
    let object = AuditObject {
        detail: crate::audit::detail(what),
        ..AuditObject::default()
    };
    crate::audit::owner(ctx, AuditAction::ChangeRules, object).await;
}

fn store_error(e: Error) -> MethodResult<Value> {
    Ok(match e {
        Error::QuotaExceeded => set_error("overQuota", None, &[]),
        Error::Conflict => set_error("alreadyExists", None, &["name"]),
        Error::NotFound => set_error("notFound", None, &[]),
        Error::Invalid(m) => set_error("invalidProperties", Some(&m), &[]),
        e => return Err(e.into()),
    })
}

async fn create_one(ctx: &Ctx<'_>, v: &Value) -> MethodResult<Result<Value, Value>> {
    let store = ctx.jmap.sieve_store()?;
    let (name, blob) = match script_props(v) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    let Some(blob) = blob else {
        return Ok(Err(set_error("invalidProperties", None, &["blobId"])));
    };
    let text = match script_text(ctx, ctx.resolve_id(&blob)).await? {
        Ok(t) => t,
        Err(e) => return Ok(Err(e)),
    };
    let named = name.flatten();
    let name = named
        .clone()
        .unwrap_or_else(|| format!("script-{}", &digest(text.as_bytes())[..8]));
    let all = store.list(ctx.scope).await?;
    if all.iter().any(|s| s.name == name) {
        return Ok(Err(set_error("alreadyExists", None, &["name"])));
    }
    if all.len() >= store.limits().max_scripts {
        return Ok(Err(set_error("overQuota", None, &[])));
    }
    let blob_id = script_blob_id(&name, &text);
    if let Err(e) = store.put(ctx.scope, &name, text).await {
        return Ok(Err(store_error(e)?));
    }
    rules_changed(ctx, &name).await;
    let mut created = json!({ "id": script_id(&name), "blobId": blob_id, "isActive": false });
    if named.is_none() {
        created["name"] = json!(name);
    }
    Ok(Ok(created))
}

async fn update_one(ctx: &Ctx<'_>, id: &str, patch: &Value) -> MethodResult<Result<Value, Value>> {
    let store = ctx.jmap.sieve_store()?;
    let all = store.list(ctx.scope).await?;
    let Some(cur) = all.iter().find(|s| script_id(&s.name) == id) else {
        return Ok(Err(set_error("notFound", None, &[])));
    };
    let (name, blob) = match script_props(patch) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    let new_name = match name {
        Some(None) => return Ok(Err(set_error("invalidProperties", None, &["name"]))),
        Some(Some(n)) if n != cur.name => Some(n),
        _ => None,
    };
    if let Some(n) = &new_name
        && all.iter().any(|s| s.name == *n)
    {
        return Ok(Err(set_error("alreadyExists", None, &["name"])));
    }
    let mut changed = Map::new();
    if let Some(blob) = blob {
        let text = match script_text(ctx, ctx.resolve_id(&blob)).await? {
            Ok(t) => t,
            Err(e) => return Ok(Err(e)),
        };
        let final_name = new_name.as_deref().unwrap_or(&cur.name);
        changed.insert("blobId".into(), json!(script_blob_id(final_name, &text)));
        if let Err(e) = store.put(ctx.scope, &cur.name, text).await {
            return Ok(Err(store_error(e)?));
        }
        rules_changed(ctx, &cur.name).await;
    }
    if let Some(n) = &new_name {
        if let Err(e) = store.rename(ctx.scope, &cur.name, n).await {
            return Ok(Err(store_error(e)?));
        }
        rules_changed(ctx, n).await;
        changed.insert("id".into(), json!(script_id(n)));
        if !changed.contains_key("blobId") {
            let text = store.get(ctx.scope, n).await?.unwrap_or_default();
            changed.insert("blobId".into(), json!(script_blob_id(n, &text)));
        }
    }
    Ok(Ok(if changed.is_empty() {
        Value::Null
    } else {
        Value::Object(changed)
    }))
}

/// `SieveScript/set`: создание, изменение (имя, текст), удаление; после успеха всех изменений —
/// `onSuccessActivateScript` или `onSuccessDeactivateScript`.
pub(crate) async fn set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let mut rest = args.clone();
    rest.retain(|k, _| k.starts_with("onSuccess"));
    let act: ActivateArgs = parse_args(rest)?;
    let mut base = args;
    base.retain(|k, _| !k.starts_with("onSuccess"));
    let a: SetArgs = parse_args(base)?;
    ctx.check_account(&a.account_id)?;
    a.check_limit(ctx.jmap.cfg.max_objects_in_set)?;
    if act.on_success_activate_script.is_some() && act.on_success_deactivate_script {
        return Err(MethodError::invalid(
            "onSuccessActivateScript and onSuccessDeactivateScript are mutually exclusive",
        ));
    }
    let store = ctx.jmap.sieve_store()?.clone();
    let old = store.state(ctx.scope).await?.to_string();
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
        let all = store.list(ctx.scope).await?;
        let err = match all.iter().find(|s| script_id(&s.name) == rid) {
            None => Some(set_error("notFound", None, &[])),
            Some(s) if s.active => Some(set_error("sieveIsActive", None, &[])),
            Some(s) => match store.delete(ctx.scope, &s.name).await {
                Ok(_) => {
                    rules_changed(ctx, &s.name).await;
                    None
                }
                Err(Error::Conflict) => Some(set_error("sieveIsActive", None, &[])),
                Err(e) => Some(store_error(e)?),
            },
        };
        match err {
            None => out.destroyed.push(id),
            Some(e) => {
                out.not_destroyed.insert(id, e);
            }
        }
    }
    let clean =
        out.not_created.is_empty() && out.not_updated.is_empty() && out.not_destroyed.is_empty();
    if clean && (act.on_success_activate_script.is_some() || act.on_success_deactivate_script) {
        let all = store.list(ctx.scope).await?;
        let target = match &act.on_success_activate_script {
            Some(id) => {
                let rid = ctx.resolve_id(id);
                Some(
                    all.iter()
                        .find(|s| script_id(&s.name) == rid)
                        .ok_or_else(|| {
                            MethodError::invalid("onSuccessActivateScript: not found")
                        })?,
                )
            }
            None => None,
        };
        let was = all.iter().find(|s| s.active);
        if was.map(|s| &s.name) != target.map(|s| &s.name) {
            store
                .activate(ctx.scope, target.map(|s| s.name.as_str()))
                .await?;
            rules_changed(ctx, target.map_or("", |s| s.name.as_str())).await;
            for (s, active) in [(was, false), (target, true)] {
                let Some(s) = s else { continue };
                let id = script_id(&s.name);
                // Созданный в этом запросе скрипт — в `created`, остальные — в `updated`.
                match out.created.values_mut().find(|c| c["id"] == id.as_str()) {
                    Some(c) => c["isActive"] = json!(active),
                    None => match out.updated.get_mut(&id) {
                        Some(Value::Object(m)) => {
                            m.insert("isActive".into(), json!(active));
                        }
                        _ => {
                            out.updated.insert(id, json!({ "isActive": active }));
                        }
                    },
                }
            }
        }
    }
    let new = store.state(ctx.scope).await?.to_string();
    Ok(out.response(&a.account_id, &old, &new))
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

/// `SieveScript/query`: фильтр `name` (подстрока без учёта регистра), `isActive`; сортировка `name`, `isActive`.
pub(crate) async fn query(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: QueryArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let store = ctx.jmap.sieve_store()?;
    let (mut name, mut active) = (None, None);
    match &a.filter {
        None | Some(Value::Null) => {}
        Some(Value::Object(f)) => {
            for (k, v) in f {
                match (k.as_str(), v) {
                    ("name", Value::String(s)) => name = Some(s.to_lowercase()),
                    ("isActive", Value::Bool(b)) => active = Some(*b),
                    _ => return Err(MethodError::with("unsupportedFilter", k.clone())),
                }
            }
        }
        Some(_) => return Err(MethodError::invalid("filter must be an object")),
    }
    let state = store.state(ctx.scope).await?;
    let mut all: Vec<ScriptInfo> = store
        .list(ctx.scope)
        .await?
        .into_iter()
        .filter(|s| active.is_none_or(|a| a == s.active))
        .filter(|s| {
            name.as_ref()
                .is_none_or(|n| s.name.to_lowercase().contains(n))
        })
        .collect();
    let sort = a.sort.as_deref().unwrap_or_default();
    if sort
        .iter()
        .any(|c| !matches!(c.property.as_str(), "name" | "isActive"))
    {
        return Err(MethodError::new("unsupportedSort"));
    }
    all.sort_by(|x, y| {
        sort.iter()
            .map(|c| {
                let o = if c.property == "name" {
                    x.name.to_lowercase().cmp(&y.name.to_lowercase())
                } else {
                    x.active.cmp(&y.active)
                };
                if c.is_ascending { o } else { o.reverse() }
            })
            .find(|o| o.is_ne())
            .unwrap_or_else(|| x.name.cmp(&y.name))
    });
    let ids: Vec<String> = all.iter().map(|s| script_id(&s.name)).collect();
    let mut out = object(json!({
        "accountId": a.account_id,
        "queryState": state.to_string(),
        "canCalculateChanges": false,
    }));
    apply_window(&ids, &a.window, ctx.jmap.cfg.max_query_limit, &mut out)?;
    Ok(out)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ValidateArgs {
    account_id: String,
    blob_id: String,
}

/// `SieveScript/validate`: проверка текста из blob без сохранения; `error` — `null` или `SetError`.
pub(crate) async fn validate(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: ValidateArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let error = match script_text(ctx, ctx.resolve_id(&a.blob_id)).await? {
        Ok(_) => Value::Null,
        Err(e) if e["type"] == "blobNotFound" => return Err(MethodError::new("blobNotFound")),
        Err(e) => e,
    };
    Ok(object(json!({ "accountId": a.account_id, "error": error })))
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use serde_json::{Value, json};
    use tp_contracts::sieve::SieveStore;

    use crate::blob::upload_id;
    use crate::testenv::Env;

    const USING: &[&str] = &["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:sieve"];
    const GOOD: &str =
        "require \"fileinto\";\nif header :contains \"subject\" \"счёт\" { fileinto \"Счета\"; }\n";
    const BAD: &str = "require \"fileinto\";\nif header :contains \"subject\" { fileinto; }\n";

    async fn call(env: &Env, method: &str, args: Value) -> Value {
        let resp = env
            .request(json!({"using": USING, "methodCalls": [[method, args, "c0"]]}))
            .await;
        let name = &resp["methodResponses"][0][0];
        assert!(name == method || name == "error", "{resp}");
        resp["methodResponses"][0][1].clone()
    }

    /// Загрузка текста (как `uploadUrl`): `blobId`.
    async fn upload(env: &Env, text: &str) -> String {
        let key = env
            .jmap
            .put_upload(env.scope, Bytes::copy_from_slice(text.as_bytes()))
            .await
            .expect("upload");
        upload_id(&key)
    }

    async fn create(env: &Env, name: &str, text: &str) -> Value {
        let blob = upload(env, text).await;
        call(
            env,
            "SieveScript/set",
            json!({"accountId": env.account(), "create": {"a": {"name": name, "blobId": blob}}}),
        )
        .await
    }

    #[tokio::test]
    async fn session_lists_capability() {
        let env = Env::new().await;
        let s = env.jmap.session(&env.principal, "https://mail.example.org");
        let acc = env.account();
        assert_eq!(s["capabilities"][USING[1]]["implementation"], "The Post");
        let caps = &s["accounts"][&acc]["accountCapabilities"][USING[1]];
        assert_eq!(caps["maxNumberScripts"], 16, "{caps}");
        assert_eq!(caps["maxSizeScript"], 128 * 1024);
        assert!(
            caps["sieveExtensions"]
                .as_array()
                .is_some_and(|e| e.contains(&json!("vacation"))),
            "{caps}"
        );
        assert_eq!(s["primaryAccounts"][USING[1]], acc);
    }

    #[tokio::test]
    async fn upload_create_activate_destroy() {
        let env = Env::new().await;
        let acc = env.account();
        let r = create(&env, "Основной", GOOD).await;
        assert!(r["notCreated"].is_null(), "{r}");
        let id = r["created"]["a"]["id"].as_str().expect("id").to_owned();
        assert_eq!(r["created"]["a"]["isActive"], false);
        assert_ne!(r["oldState"], r["newState"]);
        assert_eq!(
            env.sieve
                .get(env.scope, "Основной")
                .await
                .expect("get")
                .as_deref(),
            Some(GOOD)
        );

        // Текст скачивается по blobId.
        let got = call(
            &env,
            "SieveScript/get",
            json!({"accountId": acc, "ids": [id, "nope"]}),
        )
        .await;
        assert_eq!(got["notFound"], json!(["nope"]));
        assert_eq!(got["list"][0]["name"], "Основной", "{got}");
        let blob = got["list"][0]["blobId"].as_str().expect("blobId");
        let text = env
            .jmap
            .read_blob(env.scope, blob, 1 << 20)
            .await
            .expect("blob");
        assert_eq!(text.as_deref(), Some(GOOD.as_bytes()));

        // Активация без других изменений.
        let r = call(
            &env,
            "SieveScript/set",
            json!({"accountId": acc, "onSuccessActivateScript": id}),
        )
        .await;
        assert_eq!(r["updated"][&id]["isActive"], true, "{r}");
        let active = env
            .sieve
            .active(env.scope)
            .await
            .expect("active")
            .expect("some");
        assert_eq!(active.name, "Основной");

        // Активный скрипт не удаляется.
        let r = call(
            &env,
            "SieveScript/set",
            json!({"accountId": acc, "destroy": [id]}),
        )
        .await;
        assert_eq!(r["notDestroyed"][&id]["type"], "sieveIsActive", "{r}");

        // Выключение и удаление.
        let r = call(
            &env,
            "SieveScript/set",
            json!({"accountId": acc, "onSuccessDeactivateScript": true}),
        )
        .await;
        assert_eq!(r["updated"][&id]["isActive"], false, "{r}");
        let r = call(
            &env,
            "SieveScript/set",
            json!({"accountId": acc, "destroy": [id]}),
        )
        .await;
        assert_eq!(r["destroyed"], json!([id]), "{r}");
        assert!(env.sieve.list(env.scope).await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn create_and_activate_in_one_call() {
        let env = Env::new().await;
        let blob = upload(&env, GOOD).await;
        let r = call(
            &env,
            "SieveScript/set",
            json!({"accountId": env.account(),
                   "create": {"a": {"name": null, "blobId": blob}},
                   "onSuccessActivateScript": "#a"}),
        )
        .await;
        assert_eq!(r["created"]["a"]["isActive"], true, "{r}");
        let name = r["created"]["a"]["name"].as_str().expect("server name");
        let active = env
            .sieve
            .active(env.scope)
            .await
            .expect("active")
            .expect("some");
        assert_eq!(active.name, name);
    }

    #[tokio::test]
    async fn invalid_script_has_position() {
        let env = Env::new().await;
        let r = create(&env, "bad", BAD).await;
        let e = &r["notCreated"]["a"];
        assert_eq!(e["type"], "invalidSieve", "{r}");
        assert!(
            e["description"]
                .as_str()
                .is_some_and(|d| d.starts_with("line 2, column ")),
            "{r}"
        );
        assert!(env.sieve.list(env.scope).await.expect("list").is_empty());

        let blob = upload(&env, BAD).await;
        let v = call(
            &env,
            "SieveScript/validate",
            json!({"accountId": env.account(), "blobId": blob}),
        )
        .await;
        assert_eq!(v["error"]["type"], "invalidSieve", "{v}");
        let blob = upload(&env, GOOD).await;
        let v = call(
            &env,
            "SieveScript/validate",
            json!({"accountId": env.account(), "blobId": blob}),
        )
        .await;
        assert!(v["error"].is_null(), "{v}");
        let v = call(
            &env,
            "SieveScript/validate",
            json!({"accountId": env.account(), "blobId": "Unope_1"}),
        )
        .await;
        assert_eq!(v["type"], "blobNotFound", "{v}");
    }

    /// Пределы секции `sieve` (JMAP-35): скрипт сверх уменьшенного предела вложенности или размера отклоняется
    /// `invalidSieve` и в `/validate`, и в `/set`; capability объявляет те же пределы.
    #[tokio::test]
    async fn section_limits() {
        let limits = tp_sieve::Limits {
            max_nesting: 2,
            max_script_size: 200,
            max_redirects: 2,
            ..tp_sieve::Limits::default()
        };
        let env = Env::with_sieve_limits(limits).await;
        let acc = env.account();
        let s = env.jmap.session(&env.principal, "https://mail.example.org");
        let caps = &s["accounts"][&acc]["accountCapabilities"][USING[1]];
        assert_eq!(caps["maxSizeScript"], 200, "{caps}");
        assert_eq!(caps["maxNumberRedirects"], 2, "{caps}");

        let deep = "if true { if true { if true { if true { keep; } } } }\n";
        let large = format!("# {}\nkeep;\n", "x".repeat(300));
        for text in [deep, large.as_str()] {
            // С пределами по умолчанию скрипт верен.
            assert!(tp_sieve::check(text).is_ok(), "{text}");
            let blob = upload(&env, text).await;
            let v = call(
                &env,
                "SieveScript/validate",
                json!({"accountId": acc, "blobId": blob}),
            )
            .await;
            assert_eq!(v["error"]["type"], "invalidSieve", "{v}");
            let r = create(&env, "big", text).await;
            assert_eq!(r["notCreated"]["a"]["type"], "invalidSieve", "{r}");
            let r = create(&env, "good", GOOD).await;
            let id = r["created"]["a"]["id"].as_str().expect("id").to_owned();
            let r = call(
                &env,
                "SieveScript/set",
                json!({"accountId": acc, "update": {&id: {"blobId": blob}}}),
            )
            .await;
            assert_eq!(r["notUpdated"][&id]["type"], "invalidSieve", "{r}");
            let r = call(
                &env,
                "SieveScript/set",
                json!({"accountId": acc, "destroy": [id]}),
            )
            .await;
            assert!(r["notDestroyed"].is_null(), "{r}");
        }
        assert!(env.sieve.list(env.scope).await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn update_rename_query() {
        let env = Env::new().await;
        let acc = env.account();
        let a = create(&env, "alpha", GOOD).await["created"]["a"]["id"]
            .as_str()
            .expect("id")
            .to_owned();
        let r = create(&env, "alpha", GOOD).await;
        assert_eq!(r["notCreated"]["a"]["type"], "alreadyExists", "{r}");
        create(&env, "Beta", "keep;\n").await;

        // Новый текст и имя.
        let blob = upload(&env, "discard;\n").await;
        let r = call(
            &env,
            "SieveScript/set",
            json!({"accountId": acc, "update": {&a: {"name": "gamma", "blobId": blob}}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        let new_id = r["updated"][&a]["id"].as_str().expect("new id").to_owned();
        assert_eq!(
            env.sieve
                .get(env.scope, "gamma")
                .await
                .expect("get")
                .as_deref(),
            Some("discard;\n")
        );
        let r = call(
            &env,
            "SieveScript/set",
            json!({"accountId": acc, "update": {&new_id: {"name": "Beta"}, "nope": {"name": "x"},
                   "sX": {"isActive": true}}}),
        )
        .await;
        assert_eq!(r["notUpdated"][&new_id]["type"], "alreadyExists", "{r}");
        assert_eq!(r["notUpdated"]["nope"]["type"], "notFound", "{r}");

        call(
            &env,
            "SieveScript/set",
            json!({"accountId": acc, "onSuccessActivateScript": new_id}),
        )
        .await;
        let q = call(
            &env,
            "SieveScript/query",
            json!({"accountId": acc, "sort": [{"property": "name", "isAscending": false}],
                   "calculateTotal": true}),
        )
        .await;
        assert_eq!(q["total"], 2, "{q}");
        assert_eq!(q["ids"][0], new_id, "{q}");
        let q = call(
            &env,
            "SieveScript/query",
            json!({"accountId": acc, "filter": {"isActive": true, "name": "GAM"}}),
        )
        .await;
        assert_eq!(q["ids"], json!([new_id]), "{q}");
        let q = call(
            &env,
            "SieveScript/query",
            json!({"accountId": acc, "filter": {"operator": "AND", "conditions": []}}),
        )
        .await;
        assert_eq!(q["type"], "unsupportedFilter", "{q}");
    }
}
