//! Объекты Request/Response (RFC 8620 §3.3–3.4), ссылки на результаты (§3.7) и общие аргументы
//! стандартных методов `/get`, `/changes`, `/query`, `/queryChanges` (§5).

use std::collections::{HashMap, HashSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tp_contracts::ModSeq;
use tp_contracts::meta::Changes;

use super::error::{MethodError, MethodResult};

/// Вызов метода или ответ: `[имя, аргументы, callId]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invocation(pub String, pub Map<String, Value>, pub String);

/// Запрос API.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    /// Capability, которые использует клиент.
    pub using: Vec<String>,
    /// Вызовы по порядку.
    pub method_calls: Vec<Invocation>,
    /// Ранее созданные идентификаторы (creation id → id).
    #[serde(default)]
    pub created_ids: Option<HashMap<String, String>>,
}

/// Ответ API.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    /// Ответы методов по порядку.
    pub method_responses: Vec<Invocation>,
    /// Созданные идентификаторы — только если клиент передал `createdIds`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_ids: Option<HashMap<String, String>>,
    /// Состояние объекта сессии.
    pub session_state: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResultReference {
    result_of: String,
    name: String,
    path: String,
}

fn bad_ref(why: &str) -> MethodError {
    MethodError::with("invalidResultReference", why)
}

/// Заменяет аргументы `#имя` значениями из предыдущих ответов (RFC 8620 §3.7).
pub fn resolve_references(
    args: Map<String, Value>,
    responses: &[Invocation],
) -> MethodResult<Map<String, Value>> {
    let mut out = Map::new();
    for (key, value) in &args {
        let Some(name) = key.strip_prefix('#') else {
            out.insert(key.clone(), value.clone());
            continue;
        };
        if args.contains_key(name) {
            return Err(MethodError::invalid(format!("both {name} and #{name}")));
        }
        let r: ResultReference =
            serde_json::from_value(value.clone()).map_err(|e| bad_ref(&e.to_string()))?;
        let resp = responses
            .iter()
            .find(|i| i.2 == r.result_of)
            .ok_or_else(|| bad_ref("no response with this callId"))?;
        if resp.0 != r.name {
            return Err(bad_ref("response name mismatch"));
        }
        let found = pointer(&Value::Object(resp.1.clone()), &r.path)
            .ok_or_else(|| bad_ref("path not found"))?;
        out.insert(name.to_owned(), found);
    }
    Ok(out)
}

/// JSON Pointer (RFC 6901) с расширением JMAP: `*` применяет остаток пути к каждому элементу массива,
/// массивы-результаты объединяются.
pub fn pointer(value: &Value, path: &str) -> Option<Value> {
    if path.is_empty() {
        return Some(value.clone());
    }
    let tokens: Vec<String> = path
        .strip_prefix('/')?
        .split('/')
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect();
    eval(value, &tokens)
}

fn eval(value: &Value, tokens: &[String]) -> Option<Value> {
    let Some((first, rest)) = tokens.split_first() else {
        return Some(value.clone());
    };
    match value {
        Value::Array(items) if first == "*" => {
            let mut out = Vec::new();
            for item in items {
                match eval(item, rest)? {
                    Value::Array(inner) => out.extend(inner),
                    v => out.push(v),
                }
            }
            Some(Value::Array(out))
        }
        Value::Array(items) => eval(items.get(first.parse::<usize>().ok()?)?, rest),
        Value::Object(map) => eval(map.get(first.as_str())?, rest),
        _ => None,
    }
}

/// Аргументы метода как `T`; несоответствие — `invalidArguments`.
pub fn parse_args<T: DeserializeOwned>(args: Map<String, Value>) -> MethodResult<T> {
    serde_json::from_value(Value::Object(args)).map_err(|e| MethodError::invalid(e.to_string()))
}

/// Строка состояния JMAP из `ModSeq` учётной записи.
pub fn state_string(s: ModSeq) -> String {
    s.0.to_string()
}

/// `ModSeq` из строки состояния; чужая строка — `cannotCalculateChanges`.
pub fn parse_state(s: &str) -> MethodResult<ModSeq> {
    s.parse::<u64>()
        .map(ModSeq)
        .map_err(|_| MethodError::new("cannotCalculateChanges"))
}

/// Аргументы `/get`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetArgs {
    /// Учётная запись.
    pub account_id: String,
    /// Объекты; `None` — все (где допустимо).
    pub ids: Option<Vec<String>>,
    /// Свойства; `None` — по умолчанию.
    pub properties: Option<Vec<String>>,
}

/// Аргументы `/changes`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangesArgs {
    /// Учётная запись.
    pub account_id: String,
    /// Состояние клиента.
    pub since_state: String,
    /// Предел изменений.
    pub max_changes: Option<u64>,
}

impl ChangesArgs {
    /// Предел изменений: `maxChanges` клиента, но не больше `cap`; 0 — `invalidArguments`.
    pub fn limit(&self, cap: u32) -> MethodResult<usize> {
        limit_of(self.max_changes, cap)
    }
}

fn limit_of(max_changes: Option<u64>, cap: u32) -> MethodResult<usize> {
    match max_changes {
        Some(0) => Err(MethodError::invalid("maxChanges must be positive")),
        Some(n) => Ok(usize::try_from(n.min(u64::from(cap))).unwrap_or(usize::MAX)),
        None => Ok(cap as usize),
    }
}

/// Ответ `/changes` по журналу изменений.
pub fn changes_response(
    account_id: &str,
    old_state: &str,
    changes: &Changes<String>,
) -> Map<String, Value> {
    let v = json!({
        "accountId": account_id,
        "oldState": old_state,
        "newState": state_string(changes.new_state),
        "hasMoreChanges": changes.has_more,
        "created": changes.created,
        "updated": changes.updated,
        "destroyed": changes.destroyed,
    });
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Аргументы `/query` (общая часть).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryWindow {
    /// Позиция первого результата; отрицательная — от конца.
    #[serde(default)]
    pub position: i64,
    /// Якорь: идентификатор, от которого отсчитывается окно.
    pub anchor: Option<String>,
    /// Смещение от якоря.
    #[serde(default)]
    pub anchor_offset: i64,
    /// Размер окна.
    pub limit: Option<u64>,
    /// Вернуть `total`.
    #[serde(default)]
    pub calculate_total: bool,
}

/// Окно результатов `/query`: дополняет ответ полями `position`, `ids`, `total`, `limit`.
pub fn apply_window(
    ids: &[String],
    w: &QueryWindow,
    max_limit: u32,
    out: &mut Map<String, Value>,
) -> MethodResult<()> {
    let len = i64::try_from(ids.len()).unwrap_or(i64::MAX);
    let start = match &w.anchor {
        Some(anchor) => {
            let at = ids
                .iter()
                .position(|id| id == anchor)
                .ok_or(MethodError::new("anchorNotFound"))?;
            (i64::try_from(at).unwrap_or(i64::MAX) + w.anchor_offset).max(0)
        }
        None if w.position < 0 => (len + w.position).max(0),
        None => w.position,
    };
    let cap = u64::from(max_limit);
    let limit = w.limit.unwrap_or(cap);
    if limit > cap {
        out.insert("limit".into(), json!(cap));
    }
    let start = usize::try_from(start.min(len)).unwrap_or(0);
    let take = usize::try_from(limit.min(cap)).unwrap_or(usize::MAX);
    let page: Vec<&String> = ids.iter().skip(start).take(take).collect();
    out.insert("position".into(), json!(start));
    out.insert("ids".into(), json!(page));
    if w.calculate_total {
        out.insert("total".into(), json!(ids.len()));
    }
    Ok(())
}

/// Аргументы `/queryChanges` (общая часть).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryChangesArgs {
    /// Учётная запись.
    pub account_id: String,
    /// Состояние запроса клиента.
    pub since_query_state: String,
    /// Предел изменений.
    pub max_changes: Option<u64>,
    /// Вернуть `total`.
    #[serde(default)]
    pub calculate_total: bool,
}

impl QueryChangesArgs {
    /// Предел изменений (как у `/changes`).
    pub fn limit(&self, cap: u32) -> MethodResult<usize> {
        limit_of(self.max_changes, cap)
    }
}

/// Ответ `/queryChanges` по журналу изменений (RFC 8620 §5.6): изменённые и удалённые объекты —
/// в `removed`, изменённые и созданные, входящие в результаты, — в `added` с новой позицией.
/// Сортировка и фильтр зависят только от свойств объекта, поэтому иначе позиции не меняются.
pub fn query_changes_response(
    args: &QueryChangesArgs,
    ids_now: &[String],
    changes: &Changes<String>,
    limit: usize,
) -> MethodResult<Map<String, Value>> {
    if changes.has_more {
        return Err(MethodError::new("cannotCalculateChanges"));
    }
    let total = changes.created.len() + changes.updated.len() + changes.destroyed.len();
    if total > limit {
        return Err(MethodError::new("tooManyChanges"));
    }
    let touched: HashSet<&String> = changes.created.iter().chain(&changes.updated).collect();
    let removed: Vec<&String> = changes.updated.iter().chain(&changes.destroyed).collect();
    let added: Vec<Value> = ids_now
        .iter()
        .enumerate()
        .filter(|(_, id)| touched.contains(id))
        .map(|(i, id)| json!({ "id": id, "index": i }))
        .collect();
    let mut out = Map::new();
    out.insert("accountId".into(), json!(args.account_id));
    out.insert("oldQueryState".into(), json!(args.since_query_state));
    out.insert(
        "newQueryState".into(),
        json!(state_string(changes.new_state)),
    );
    out.insert("removed".into(), json!(removed));
    out.insert("added".into(), json!(added));
    if args.calculate_total {
        out.insert("total".into(), json!(ids_now.len()));
    }
    Ok(out)
}

/// Изменения с идентификаторами JMAP.
pub fn map_changes<T>(c: Changes<T>, id: impl Fn(&T) -> String) -> Changes<String> {
    Changes {
        created: c.created.iter().map(&id).collect(),
        updated: c.updated.iter().map(&id).collect(),
        destroyed: c.destroyed.iter().map(&id).collect(),
        new_state: c.new_state,
        has_more: c.has_more,
    }
}

/// Оставляет в объекте только `properties` (и всегда `id`).
pub fn select(mut obj: Map<String, Value>, properties: Option<&[String]>) -> Map<String, Value> {
    if let Some(props) = properties {
        obj.retain(|k, _| k == "id" || props.iter().any(|p| p == k));
    }
    obj
}

/// Объект JSON из `json!({...})`.
pub fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}
