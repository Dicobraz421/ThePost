//! Схема Admin API для движка экранов (TUI-02, решение 118): операции, параметры, поля тела и колонки ответа
//! берутся из `tp_admin::server::openapi_spec()` (OpenAPI 3.1). Своего списка операций у интерфейса нет: новая
//! операция Admin API появляется в меню и на экранах движка без правки кода.

use std::sync::OnceLock;

use serde_json::{Map, Value};
use tp_admin_client::ops::Op;

/// Где передаётся значение поля.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// Параметр пути (`{имя}`).
    Path,
    /// Параметр запроса (`?имя=`).
    Query,
    /// Поле тела JSON.
    Body,
}

/// Тип значения поля формы.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Строка.
    Text,
    /// Целое.
    Integer,
    /// Число.
    Number,
    /// Да или нет.
    Boolean,
    /// Список строк — через запятую.
    List,
    /// Прочее (объект, варианты `oneOf`, вложенные списки) — значение JSON.
    Json,
}

/// Поле формы или параметр операции.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// Имя, как в схеме.
    pub name: String,
    /// Где передаётся.
    pub location: Location,
    /// Тип.
    pub kind: Kind,
    /// Обязательное.
    pub required: bool,
    /// Допускает `null`.
    pub nullable: bool,
    /// Допустимые значения (перечисление); пусто — любые.
    pub choices: Vec<String>,
    /// Наименьшее значение числа.
    pub minimum: Option<f64>,
    /// Описание из схемы.
    pub description: String,
}

/// Колонка списка или поле карточки.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// Имя поля ответа.
    pub name: String,
    /// Тип.
    pub kind: Kind,
    /// Описание из схемы.
    pub description: String,
}

/// Продолжение страницы из поля ответа: значение поля `field` — параметр запроса `param` следующей страницы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// Поле ответа (`next`, `next_offset`).
    pub field: String,
    /// Параметр запроса (`after`, `offset`).
    pub param: String,
}

/// Ответ операции.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// Тела нет (204).
    Empty,
    /// Список записей: `wrapper` — поле объекта со списком (`None` — ответ сам массив), колонки записей,
    /// продолжение из поля ответа (листание по ссылке `Link` — у клиента).
    List {
        /// Поле объекта со списком.
        wrapper: Option<String>,
        /// Колонки записи.
        columns: Vec<Column>,
        /// Продолжение из поля ответа.
        cursor: Option<Cursor>,
    },
    /// Одна запись.
    Record {
        /// Поля записи.
        fields: Vec<Column>,
    },
    /// Прочее: строка, число, объект без свойств.
    Other,
}

/// Тело запроса.
#[derive(Debug, Clone, PartialEq)]
pub struct Body {
    /// Поля объекта; тело не объект — одно поле `Json` с именем [`RAW_BODY`].
    pub fields: Vec<Field>,
    /// Тело обязательно.
    pub required: bool,
}

/// Имя поля тела, когда тело не объект.
pub const RAW_BODY: &str = "тело";

/// Операция Admin API.
#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    /// `operationId`.
    pub id: String,
    /// Метод HTTP.
    pub method: String,
    /// Шаблон пути.
    pub path: String,
    /// Раздел (первый тег).
    pub tag: String,
    /// Описание из схемы.
    pub summary: String,
    /// Параметры пути (по порядку шаблона) и запроса.
    pub params: Vec<Field>,
    /// Тело запроса.
    pub body: Option<Body>,
    /// Ответ.
    pub response: Response,
}

impl Operation {
    /// Операция для клиента Admin API.
    pub fn op(&'static self) -> Op {
        Op {
            method: &self.method,
            path: &self.path,
        }
    }

    /// Заголовок: первое предложение описания.
    pub fn title(&self) -> String {
        let first = self.summary.lines().next().unwrap_or_default();
        let end = first.find(". ").map_or(first.len(), |i| i + 1);
        let t = first[..end].trim();
        if t.is_empty() {
            format!("{} {}", self.method, self.path)
        } else {
            t.to_owned()
        }
    }

    /// Имена параметров пути по порядку шаблона.
    pub fn path_names(&self) -> Vec<&str> {
        self.path
            .split('/')
            .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            .collect()
    }

    /// Параметры запроса.
    pub fn query(&self) -> impl Iterator<Item = &Field> {
        self.params.iter().filter(|f| f.location == Location::Query)
    }

    /// Ответ — список записей.
    pub fn is_list(&self) -> bool {
        matches!(self.response, Response::List { .. })
    }
}

/// Все операции схемы Admin API.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    /// Операции в порядке путей схемы.
    pub ops: Vec<Operation>,
}

/// Операции `tp_admin::server::openapi_spec()`, разобранные один раз.
pub fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let spec = serde_json::to_value(tp_admin::server::openapi_spec()).unwrap_or(Value::Null);
        Catalog::from_spec(&spec)
    })
}

/// Методы HTTP операций в порядке показа.
const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

impl Catalog {
    /// Операции из спецификации OpenAPI (JSON); непонятные части схемы становятся полями `Json`.
    pub fn from_spec(spec: &Value) -> Self {
        let empty = Map::new();
        let schemas = spec
            .pointer("/components/schemas")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let r = Resolver::new(schemas);
        let mut ops = Vec::new();
        for (path, item) in spec
            .get("paths")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            for method in METHODS {
                if let Some(o) = item.get(method) {
                    ops.push(r.operation(path, method, o, item.get("parameters")));
                }
            }
        }
        Self { ops }
    }

    /// Операция по `operationId`.
    pub fn get(&self, id: &str) -> Option<&Operation> {
        self.ops.iter().find(|o| o.id == id)
    }

    /// Операция по методу и шаблону пути.
    pub fn find(&self, method: &str, path: &str) -> Option<&Operation> {
        self.ops
            .iter()
            .find(|o| o.method.eq_ignore_ascii_case(method) && o.path == path)
    }

    /// Разделы (теги) в порядке первого появления.
    pub fn tags(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for o in &self.ops {
            if !out.contains(&o.tag.as_str()) {
                out.push(&o.tag);
            }
        }
        out
    }
}

/// Колонки, которые показываются первыми (по порядку), если есть в записи; `id` — последней.
const PREFERRED: [&str; 14] = [
    "login",
    "name",
    "email",
    "address",
    "pattern",
    "display_name",
    "sender",
    "subject",
    "label",
    "at",
    "held_at",
    "actor",
    "method",
    "enabled",
];

/// Свойства объекта и имена обязательных.
pub(crate) type Props<'a> = (Vec<(&'a str, &'a Value)>, Vec<&'a str>);

/// Разбор JSON Schema: ссылки `$ref` на схемы OpenAPI (`#/components/schemas/`) и JSON Schema (`#/$defs/`),
/// объекты с `allOf`, вид значения поля.
pub(crate) struct Resolver<'a> {
    schemas: &'a Map<String, Value>,
}

impl<'a> Resolver<'a> {
    /// Разбор со схемами `schemas` (`components/schemas` или `$defs`).
    pub(crate) fn new(schemas: &'a Map<String, Value>) -> Self {
        Self { schemas }
    }

    /// Схема по ссылке `$ref`, цепочка — до 16 переходов.
    pub(crate) fn deref(&self, mut v: &'a Value) -> &'a Value {
        for _ in 0..16 {
            let Some(name) = v.get("$ref").and_then(Value::as_str).and_then(|r| {
                r.strip_prefix("#/components/schemas/")
                    .or_else(|| r.strip_prefix("#/$defs/"))
            }) else {
                break;
            };
            match self.schemas.get(name) {
                Some(s) => v = s,
                None => break,
            }
        }
        v
    }

    /// Свойства объекта (с `allOf`) и список обязательных; не объект — `None`.
    pub(crate) fn object(&self, v: &'a Value) -> Option<Props<'a>> {
        let v = self.deref(v);
        if let Some(parts) = v.get("allOf").and_then(Value::as_array) {
            let mut props = Vec::new();
            let mut req = Vec::new();
            for p in parts {
                if let Some((pp, rr)) = self.object(p) {
                    props.extend(pp);
                    req.extend(rr);
                }
            }
            return (!props.is_empty()).then_some((props, req));
        }
        let props = v.get("properties").and_then(Value::as_object)?;
        let req = v
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        Some((props.iter().map(|(k, s)| (k.as_str(), s)).collect(), req))
    }

    /// Тип значения: вид, допускает ли `null`, перечисление.
    pub(crate) fn kind(&self, v: &'a Value) -> (Kind, bool, Vec<String>) {
        let v = self.deref(v);
        if let Some(vars) = v
            .get("oneOf")
            .or_else(|| v.get("anyOf"))
            .and_then(Value::as_array)
        {
            let is_null =
                |s: &Value| self.deref(s).get("type").and_then(Value::as_str) == Some("null");
            let nullable = vars.iter().any(is_null);
            let rest: Vec<&Value> = vars.iter().filter(|s| !is_null(s)).collect();
            if let [one] = rest.as_slice() {
                let (k, n, c) = self.kind(one);
                return (k, n || nullable, c);
            }
            let mut choices = Vec::new();
            for s in &rest {
                match self.kind(s) {
                    (Kind::Text, _, c) if !c.is_empty() => choices.extend(c),
                    _ => return (Kind::Json, nullable, Vec::new()),
                }
            }
            return (Kind::Text, nullable, choices);
        }
        let (types, nullable) = match v.get("type") {
            Some(Value::String(t)) => (vec![t.as_str()], false),
            Some(Value::Array(ts)) => {
                let ts: Vec<&str> = ts.iter().filter_map(Value::as_str).collect();
                let nullable = ts.contains(&"null");
                (ts.into_iter().filter(|t| *t != "null").collect(), nullable)
            }
            _ => (Vec::new(), false),
        };
        let choices: Vec<String> = v
            .get("enum")
            .and_then(Value::as_array)
            .map(|e| {
                e.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let kind = match types.as_slice() {
            ["string"] => Kind::Text,
            ["integer"] => Kind::Integer,
            ["number"] => Kind::Number,
            ["boolean"] => Kind::Boolean,
            ["array"] => match v.get("items").map(|i| self.kind(i)) {
                Some((Kind::Text, _, c)) if c.is_empty() => Kind::List,
                _ => Kind::Json,
            },
            [] if !choices.is_empty() => Kind::Text,
            _ => Kind::Json,
        };
        (kind, nullable, choices)
    }

    fn field(&self, name: &str, location: Location, schema: &'a Value, required: bool) -> Field {
        let (kind, nullable, choices) = self.kind(schema);
        let s = self.deref(schema);
        Field {
            name: name.to_owned(),
            location,
            kind,
            required,
            nullable,
            choices,
            minimum: s.get("minimum").and_then(Value::as_f64),
            description: describe(schema).or_else(|| describe(s)).unwrap_or_default(),
        }
    }

    fn columns(&self, v: &'a Value) -> Option<Vec<Column>> {
        let (props, _) = self.object(v)?;
        let mut cols: Vec<Column> = props
            .into_iter()
            .map(|(name, s)| Column {
                name: name.to_owned(),
                kind: self.kind(s).0,
                description: describe(s)
                    .or_else(|| describe(self.deref(s)))
                    .unwrap_or_default(),
            })
            .collect();
        let rank = |c: &Column| match PREFERRED.iter().position(|p| *p == c.name) {
            Some(i) => i,
            None if c.name == "id" => PREFERRED.len() + 1,
            None => PREFERRED.len(),
        };
        cols.sort_by_key(rank);
        Some(cols)
    }

    fn operation(
        &self,
        path: &str,
        method: &str,
        o: &'a Value,
        common: Option<&'a Value>,
    ) -> Operation {
        let mut params = Vec::new();
        let all = common
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .chain(
                o.get("parameters")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten(),
            );
        for p in all {
            let p = self.deref(p);
            let Some(name) = p.get("name").and_then(Value::as_str) else {
                continue;
            };
            let location = match p.get("in").and_then(Value::as_str) {
                Some("path") => Location::Path,
                Some("query") => Location::Query,
                _ => continue,
            };
            let schema = p.get("schema").unwrap_or(&Value::Null);
            let required = location == Location::Path
                || p.get("required").and_then(Value::as_bool).unwrap_or(false);
            let mut f = self.field(name, location, schema, required);
            if let Some(d) = describe(p) {
                f.description = d;
            }
            params.retain(|x: &Field| x.name != f.name || x.location != f.location);
            params.push(f);
        }
        let order: Vec<&str> = path
            .split('/')
            .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            .collect();
        params.sort_by_key(|f| match f.location {
            Location::Path => order
                .iter()
                .position(|n| *n == f.name)
                .unwrap_or(order.len()),
            _ => order.len() + 1,
        });
        let body = o.get("requestBody").map(|rb| {
            let required = rb.get("required").and_then(Value::as_bool).unwrap_or(false);
            let schema = json_schema(rb);
            let fields = match schema.and_then(|s| self.object(s)) {
                Some((props, req)) => props
                    .into_iter()
                    .map(|(n, s)| self.field(n, Location::Body, s, req.contains(&n)))
                    .collect(),
                None => vec![Field {
                    name: RAW_BODY.to_owned(),
                    location: Location::Body,
                    kind: Kind::Json,
                    required,
                    nullable: false,
                    choices: Vec::new(),
                    minimum: None,
                    description: schema.and_then(describe).unwrap_or_default(),
                }],
            };
            Body { fields, required }
        });
        let response = self.response(path, method, o, &params);
        Operation {
            id: o
                .get("operationId")
                .and_then(Value::as_str)
                .map_or_else(|| format!("{method} {path}"), str::to_owned),
            method: method.to_ascii_uppercase(),
            path: path.to_owned(),
            tag: o
                .pointer("/tags/0")
                .and_then(Value::as_str)
                .unwrap_or("прочее")
                .to_owned(),
            summary: o
                .get("summary")
                .or_else(|| o.get("description"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            params,
            body,
            response,
        }
    }

    fn response(&self, path: &str, method: &str, o: &'a Value, params: &[Field]) -> Response {
        let Some(responses) = o.get("responses").and_then(Value::as_object) else {
            return Response::Other;
        };
        let Some((_, ok)) = responses.iter().find(|(code, _)| code.starts_with('2')) else {
            return Response::Other;
        };
        let Some(schema) = json_schema(self.deref(ok)) else {
            return Response::Empty;
        };
        let s = self.deref(schema);
        if s.get("type").and_then(Value::as_str) == Some("array") {
            return match s.get("items").and_then(|i| self.columns(i)) {
                Some(columns) => Response::List {
                    wrapper: None,
                    columns,
                    cursor: None,
                },
                None => Response::Other,
            };
        }
        let Some((props, _)) = self.object(s) else {
            return Response::Other;
        };
        // Страница-обёртка: GET коллекции (последний сегмент пути — не параметр), ровно одно поле — список записей.
        let collection = method == "get" && !path.ends_with('}');
        let lists: Vec<(&str, Vec<Column>)> = props
            .iter()
            .filter_map(|(n, p)| {
                let p = self.deref(p);
                (p.get("type").and_then(Value::as_str) == Some("array"))
                    .then(|| p.get("items").and_then(|i| self.columns(i)))
                    .flatten()
                    .map(|c| (*n, c))
            })
            .collect();
        if collection && let [(wrapper, columns)] = lists.as_slice() {
            let has = |q: &str| {
                params
                    .iter()
                    .any(|f| f.location == Location::Query && f.name == q)
            };
            let cursor = props.iter().find_map(|(n, _)| {
                let rest = n.strip_prefix("next")?;
                let param = match rest.strip_prefix('_') {
                    Some(p) if has(p) => p,
                    _ if has("after") => "after",
                    _ => return None,
                };
                Some(Cursor {
                    field: (*n).to_owned(),
                    param: param.to_owned(),
                })
            });
            return Response::List {
                wrapper: Some((*wrapper).to_owned()),
                columns: columns.clone(),
                cursor,
            };
        }
        match self.columns(s) {
            Some(fields) => Response::Record { fields },
            None => Response::Other,
        }
    }
}

/// Схема содержимого `application/json` запроса или ответа.
fn json_schema(v: &Value) -> Option<&Value> {
    v.pointer("/content/application~1json/schema")
}

/// Описание схемы: первая строка `description`.
pub(crate) fn describe(v: &Value) -> Option<String> {
    v.get("description")
        .and_then(Value::as_str)
        .map(|d| d.lines().next().unwrap_or_default().trim().to_owned())
        .filter(|d| !d.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_list_and_form() {
        let c = catalog();
        let list = c.get("list_accounts").expect("list_accounts");
        assert_eq!(list.method, "GET");
        let Response::List {
            wrapper, columns, ..
        } = &list.response
        else {
            panic!("не список: {:?}", list.response);
        };
        assert_eq!(wrapper, &None);
        assert_eq!(columns.first().map(|c| c.name.as_str()), Some("login"));
        assert_eq!(columns.last().map(|c| c.name.as_str()), Some("id"));
        let q: Vec<&str> = list.query().map(|f| f.name.as_str()).collect();
        assert_eq!(q, ["login", "prefix", "after", "limit"]);
        assert_eq!(
            list.query().find(|f| f.name == "limit").map(|f| f.kind),
            Some(Kind::Integer)
        );

        let create = c.get("create_account").expect("create_account");
        let body = create.body.as_ref().expect("тело");
        let login = body
            .fields
            .iter()
            .find(|f| f.name == "login")
            .expect("login");
        assert!(login.required && login.kind == Kind::Text);
        let name = body
            .fields
            .iter()
            .find(|f| f.name == "display_name")
            .expect("display_name");
        assert!(!name.required && name.nullable);

        let patch = c.get("patch_account").expect("patch_account");
        let fields = &patch.body.as_ref().expect("тело").fields;
        let kind = |n: &str| fields.iter().find(|f| f.name == n).map(|f| f.kind);
        assert_eq!(kind("enabled"), Some(Kind::Boolean));
        assert_eq!(kind("aliases"), Some(Kind::List));
        assert_eq!(patch.path_names(), ["org", "account"]);
    }

    #[test]
    fn wrapped_pages_and_cursor() {
        let c = catalog();
        let audit = c.get("search_audit").expect("search_audit");
        assert_eq!(
            audit.response,
            Response::List {
                wrapper: Some("entries".into()),
                columns: match &audit.response {
                    Response::List { columns, .. } => columns.clone(),
                    _ => Vec::new(),
                },
                cursor: Some(Cursor {
                    field: "next".into(),
                    param: "after".into()
                }),
            }
        );
        let trash = c.get("list_trash").expect("list_trash");
        let Response::List {
            wrapper, cursor, ..
        } = &trash.response
        else {
            panic!("не список");
        };
        assert_eq!(wrapper.as_deref(), Some("items"));
        assert_eq!(cursor.as_ref().map(|c| c.param.as_str()), Some("offset"));
        // Запись с полем-списком (участники группы) — карточка, а не список.
        assert!(matches!(
            c.get("get_group").map(|o| &o.response),
            Some(Response::Record { .. })
        ));
        assert_eq!(
            c.get("delete_group").map(|o| &o.response),
            Some(&Response::Empty)
        );
    }

    #[test]
    fn enums_and_variants() {
        let c = catalog();
        let route = c.get("put_route").expect("put_route");
        let tls = route
            .body
            .as_ref()
            .and_then(|b| b.fields.iter().find(|f| f.name == "tls"));
        // TlsRequirement: строки и объект — значение JSON.
        assert_eq!(tls.map(|f| f.kind), Some(Kind::Json));
    }

    #[test]
    fn title_is_first_sentence() {
        let c = catalog();
        assert_eq!(
            c.get("get_group").map(Operation::title).as_deref(),
            Some("Группа.")
        );
    }
}
