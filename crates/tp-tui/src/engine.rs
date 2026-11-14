//! Общий движок экранов по схеме Admin API (TUI-02): для каждой операции — список (колонки, листание, фильтры
//! параметров запроса), карточка, форма (поля тела, типы, обязательность, перечисления) или подтверждение
//! удаления; отказ problem+json показывается у поля формы.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use tp_admin::server::Problem;

use crate::schema::{Catalog, Column, Field, Kind, Location, Operation, RAW_BODY, Response};

/// Значения параметров пути, известные экрану: организация, выбранная запись.
pub type Context = BTreeMap<String, String>;

/// Вид экрана операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenKind {
    /// Список записей.
    List,
    /// Карточка записи.
    Card,
    /// Форма запроса.
    Form,
    /// Подтверждение удаления.
    Confirm,
}

/// Экран, который движок строит для операции.
pub fn screen_kind(op: &Operation) -> ScreenKind {
    match op.method.as_str() {
        "GET" if op.is_list() => ScreenKind::List,
        "GET" => ScreenKind::Card,
        "DELETE" => ScreenKind::Confirm,
        _ => ScreenKind::Form,
    }
}

/// Запрос к Admin API, собранный экраном.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// Операция.
    pub op: &'static Operation,
    /// Параметры пути по порядку шаблона.
    pub params: Vec<String>,
    /// Параметры запроса.
    pub query: Vec<(String, String)>,
    /// Тело JSON.
    pub body: Option<Value>,
}

impl Request {
    /// Запрос без тела: параметры пути из `ctx`, запрос `query`; нет параметра пути — его имя.
    pub fn new(
        op: &'static Operation,
        ctx: &Context,
        query: Vec<(String, String)>,
    ) -> Result<Self, String> {
        let params = op
            .path_names()
            .into_iter()
            .map(|n| ctx.get(n).cloned().ok_or_else(|| n.to_owned()))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            op,
            params,
            query,
            body: None,
        })
    }

    /// Значение параметра пути по имени.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.op
            .path_names()
            .iter()
            .position(|n| *n == name)
            .and_then(|i| self.params.get(i))
            .map(String::as_str)
    }
}

/// Поле формы со значением и ошибкой.
#[derive(Debug, Clone, PartialEq)]
pub struct FormField {
    /// Поле схемы.
    pub field: &'static Field,
    /// Введённое значение (текст).
    pub value: String,
    /// Ошибка у поля: проверка формы или `detail` отказа Admin API.
    pub error: Option<String>,
}

/// Форма операции: параметры пути, которых нет в контексте, параметры запроса и поля тела.
#[derive(Debug, Clone, PartialEq)]
pub struct Form {
    /// Операция.
    pub op: &'static Operation,
    /// Поля.
    pub fields: Vec<FormField>,
    /// Поле под курсором.
    pub focus: usize,
    /// Ошибка формы целиком (отказ без поля).
    pub error: Option<String>,
    ctx: Context,
}

impl Form {
    /// Форма `op`: параметры пути из `ctx` не спрашиваются; `only_query` — только параметры запроса (фильтр
    /// списка); `prefill` — начальные значения по имени поля (запись списка, прежний фильтр).
    pub fn new(
        op: &'static Operation,
        ctx: &Context,
        only_query: bool,
        prefill: &Map<String, Value>,
    ) -> Self {
        let mut fields = Vec::new();
        for f in &op.params {
            let skip = match f.location {
                Location::Path => only_query || ctx.contains_key(&f.name),
                _ => false,
            };
            if !skip {
                fields.push(f);
            }
        }
        if !only_query && let Some(body) = &op.body {
            fields.extend(&body.fields);
        }
        let fields = fields
            .into_iter()
            .map(|field| FormField {
                field,
                value: prefill
                    .get(&field.name)
                    .map(|v| text(v, field.kind))
                    .unwrap_or_default(),
                error: None,
            })
            .collect();
        Self {
            op,
            fields,
            focus: 0,
            error: None,
            ctx: ctx.clone(),
        }
    }

    /// Значение поля по имени.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|f| f.field.name == name)
            .map(|f| f.value.as_str())
    }

    /// Задать значение поля по имени; нет поля — `false`.
    pub fn set(&mut self, name: &str, value: &str) -> bool {
        match self.fields.iter_mut().find(|f| f.field.name == name) {
            Some(f) => {
                value.clone_into(&mut f.value);
                true
            }
            None => false,
        }
    }

    /// Проверка и сборка запроса; ошибки — у полей, тогда `None`.
    pub fn request(&mut self) -> Option<Request> {
        self.error = None;
        let mut ok = true;
        let mut ctx = self.ctx.clone();
        let mut query = Vec::new();
        let mut body = Map::new();
        for ff in &mut self.fields {
            ff.error = None;
            let f = ff.field;
            let raw = ff.value.trim();
            if raw.is_empty() {
                if f.required {
                    ff.error = Some("обязательное поле".to_owned());
                    ok = false;
                }
                continue;
            }
            match parse(f, raw) {
                Err(e) => {
                    ff.error = Some(e);
                    ok = false;
                }
                Ok(v) => match f.location {
                    Location::Path => {
                        ctx.insert(f.name.clone(), raw.to_owned());
                    }
                    Location::Query => query.push((f.name.clone(), raw.to_owned())),
                    Location::Body => {
                        body.insert(f.name.clone(), v);
                    }
                },
            }
        }
        if !ok {
            return None;
        }
        let mut req = match Request::new(self.op, &ctx, query) {
            Ok(r) => r,
            Err(missing) => {
                self.error = Some(format!("нет параметра пути {missing}"));
                return None;
            }
        };
        if let Some(b) = &self.op.body {
            req.body = match body.remove(RAW_BODY) {
                Some(raw) if b.fields.len() == 1 && b.fields[0].name == RAW_BODY => Some(raw),
                _ if body.is_empty() && !b.required => None,
                _ => Some(Value::Object(body)),
            };
        }
        Some(req)
    }

    /// Запрос по введённым значениям без проверки — для строки команды CLI; нет значения параметра пути —
    /// `<имя>`.
    pub fn preview(&self) -> Request {
        let mut ctx = self.ctx.clone();
        let mut query = Vec::new();
        let mut body = Map::new();
        for ff in &self.fields {
            let f = ff.field;
            let raw = ff.value.trim();
            match f.location {
                Location::Path if !raw.is_empty() => {
                    ctx.insert(f.name.clone(), raw.to_owned());
                }
                _ if raw.is_empty() => {}
                Location::Path => {}
                Location::Query => query.push((f.name.clone(), raw.to_owned())),
                Location::Body => {
                    let v = parse(f, raw).unwrap_or_else(|_| Value::String(raw.to_owned()));
                    body.insert(f.name.clone(), v);
                }
            }
        }
        for n in self.op.path_names() {
            ctx.entry(n.to_owned()).or_insert_with(|| format!("<{n}>"));
        }
        let mut req = Request::new(self.op, &ctx, query).unwrap_or_else(|_| Request {
            op: self.op,
            params: Vec::new(),
            query: Vec::new(),
            body: None,
        });
        req.body = (!body.is_empty()).then_some(Value::Object(body));
        req
    }

    /// Отказ Admin API: `detail` — у поля, имя которого в нём названо (`поле: …`, `` `поле` ``, слово), иначе у
    /// формы целиком.
    pub fn apply_problem(&mut self, problem: &Problem) {
        let detail = problem
            .detail
            .clone()
            .unwrap_or_else(|| format!("{} {}", problem.status, problem.title));
        let at = self.field_for(&detail);
        match at {
            Some(i) => {
                self.fields[i].error = Some(detail);
                self.focus = i;
            }
            None => self.error = Some(detail),
        }
    }

    fn field_for(&self, detail: &str) -> Option<usize> {
        let head = detail.split(':').next().unwrap_or_default().trim();
        let words: Vec<&str> = detail
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty())
            .collect();
        let find =
            |pred: &dyn Fn(&str) -> bool| self.fields.iter().position(|f| pred(&f.field.name));
        find(&|n| n == head)
            .or_else(|| find(&|n| detail.contains(&format!("`{n}`"))))
            .or_else(|| find(&|n| words.contains(&n)))
    }
}

/// Значение поля из текста формы.
pub fn parse(f: &Field, raw: &str) -> Result<Value, String> {
    let v = match f.kind {
        Kind::Text => Value::String(raw.to_owned()),
        Kind::Integer => {
            let n: i64 = raw.parse().map_err(|_| "нужно целое число".to_owned())?;
            if f.minimum.is_some_and(|m| (n as f64) < m) {
                return Err(format!("не меньше {}", f.minimum.unwrap_or_default()));
            }
            Value::from(n)
        }
        Kind::Number => {
            let n: f64 = raw
                .replace(',', ".")
                .parse()
                .map_err(|_| "нужно число".to_owned())?;
            Value::from(n)
        }
        Kind::Boolean => match raw.to_lowercase().as_str() {
            "да" | "true" | "1" | "+" => Value::Bool(true),
            "нет" | "false" | "0" | "-" => Value::Bool(false),
            _ => return Err("да или нет".to_owned()),
        },
        Kind::List => Value::Array(
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_owned()))
                .collect(),
        ),
        Kind::Json => serde_json::from_str(raw).map_err(|e| format!("не JSON: {e}"))?,
    };
    if !f.choices.is_empty() && !f.choices.iter().any(|c| Some(c.as_str()) == v.as_str()) {
        return Err(format!("одно из: {}", f.choices.join(", ")));
    }
    Ok(v)
}

/// Текст значения для поля формы вида `kind`.
pub fn text(v: &Value, kind: Kind) -> String {
    match (v, kind) {
        (Value::Null, _) => String::new(),
        (Value::Bool(b), _) => if *b { "да" } else { "нет" }.to_owned(),
        (Value::String(s), _) => s.clone(),
        (Value::Array(a), Kind::List) => a
            .iter()
            .map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_owned))
            .collect::<Vec<_>>()
            .join(", "),
        (other, _) => other.to_string(),
    }
}

/// Текст ячейки списка или карточки.
pub fn cell(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "—".to_owned(),
        Some(Value::Bool(true)) => "да".to_owned(),
        Some(Value::Bool(false)) => "нет".to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        Some(other) => other.to_string(),
    }
}

/// Записи страницы списка `op` из ответа и параметры запроса следующей страницы по полю ответа (курсор).
pub fn page(op: &Operation, resp: &Value) -> (Vec<Value>, Option<(String, String)>) {
    let Response::List {
        wrapper, cursor, ..
    } = &op.response
    else {
        return (Vec::new(), None);
    };
    let rows = match wrapper {
        Some(w) => resp.get(w),
        None => Some(resp),
    }
    .and_then(Value::as_array)
    .cloned()
    .unwrap_or_default();
    let next = cursor.as_ref().and_then(|c| match resp.get(&c.field) {
        None | Some(Value::Null) => None,
        Some(v) => Some((c.param.clone(), cell(Some(v)))),
    });
    (rows, next)
}

/// Колонки списка `op`, которые помещаются в ширину: простые значения, не JSON.
pub fn list_columns(op: &Operation) -> Vec<&Column> {
    match &op.response {
        Response::List { columns, .. } => columns.iter().filter(|c| c.kind != Kind::Json).collect(),
        _ => Vec::new(),
    }
}

/// Действия над записью списка `list`: операции, путь которых — путь списка и ещё один параметр (`…/{id}`), или
/// его продолжение (`…/{id}/…`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ItemOps {
    /// Параметр пути записи.
    pub param: Option<&'static str>,
    /// Просмотр (`GET …/{id}`).
    pub view: Option<&'static Operation>,
    /// Правка (`PATCH` или `PUT …/{id}`).
    pub edit: Option<&'static Operation>,
    /// Удаление (`DELETE …/{id}`).
    pub delete: Option<&'static Operation>,
    /// Создание (`POST` на путь списка).
    pub create: Option<&'static Operation>,
    /// Пароль (`POST …/{id}/app-passwords`).
    pub password: Option<&'static Operation>,
}

/// Действия над записями списка `list` в каталоге `c`.
pub fn item_ops(c: &'static Catalog, list: &Operation) -> ItemOps {
    let mut out = ItemOps {
        create: c.find("POST", &list.path),
        ..ItemOps::default()
    };
    let prefix = format!("{}/{{", list.path);
    for o in &c.ops {
        let Some(rest) = o.path.strip_prefix(&prefix) else {
            continue;
        };
        let Some((param, tail)) = rest.split_once('}') else {
            continue;
        };
        if out.param.is_none() {
            let start = o.path.len() - rest.len();
            out.param = Some(&o.path[start..start + param.len()]);
        }
        match (o.method.as_str(), tail) {
            ("GET", "") => out.view = Some(o),
            ("PATCH", "") => out.edit = Some(o),
            ("PUT", "") if out.edit.is_none() => out.edit = Some(o),
            ("DELETE", "") => out.delete = Some(o),
            ("POST", "/app-passwords") => out.password = Some(o),
            _ => {}
        }
    }
    out
}

/// Ключ записи для параметра пути `param`: поле `id`, поле с именем параметра или `name`, `pattern`.
pub fn row_key(row: &Value, param: &str) -> Option<String> {
    ["id", param, "name", "pattern"]
        .iter()
        .find_map(|k| row.get(*k).and_then(Value::as_str))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::catalog;

    fn ctx() -> Context {
        Context::from([("org".to_owned(), "o1".to_owned())])
    }

    /// Для каждой операции схемы движок строит экран: список — с колонками, форма — с полями, запрос собирается
    /// из параметров пути контекста.
    #[test]
    fn every_operation_has_a_screen() {
        let c = catalog();
        assert!(c.ops.len() > 50, "операций: {}", c.ops.len());
        for op in &c.ops {
            let mut ctx = ctx();
            for n in op.path_names() {
                ctx.entry(n.to_owned()).or_insert_with(|| format!("{n}-1"));
            }
            match screen_kind(op) {
                ScreenKind::List => assert!(!list_columns(op).is_empty(), "{}: нет колонок", op.id),
                ScreenKind::Card | ScreenKind::Confirm => {
                    let r = Request::new(op, &ctx, Vec::new()).expect("запрос");
                    assert_eq!(r.params.len(), op.path_names().len(), "{}", op.id);
                }
                ScreenKind::Form => {
                    let mut f = Form::new(op, &ctx, false, &Map::new());
                    for ff in &mut f.fields {
                        if ff.field.required {
                            ff.value = sample(ff.field);
                        }
                    }
                    let req = f.request();
                    assert!(
                        req.is_some(),
                        "{}: форма не собирается: {:?} {:?}",
                        op.id,
                        f.error,
                        f.fields
                            .iter()
                            .filter_map(|x| x.error.as_ref())
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
    }

    fn sample(f: &Field) -> String {
        if let Some(c) = f.choices.first() {
            return c.clone();
        }
        match f.kind {
            Kind::Text | Kind::List => "x".to_owned(),
            Kind::Integer | Kind::Number => "1".to_owned(),
            Kind::Boolean => "да".to_owned(),
            Kind::Json => "{}".to_owned(),
        }
    }

    #[test]
    fn form_builds_typed_body() {
        let c = catalog();
        let op = c.get("patch_account").expect("patch_account");
        let mut ctx = ctx();
        ctx.insert("account".into(), "a1".into());
        let mut f = Form::new(op, &ctx, false, &Map::new());
        assert!(f.set("enabled", "нет"));
        assert!(f.set("aliases", "a@b.test, c@b.test"));
        let req = f.request().expect("запрос");
        assert_eq!(req.params, ["o1", "a1"]);
        assert_eq!(
            req.body,
            Some(serde_json::json!({"enabled": false, "aliases": ["a@b.test", "c@b.test"]}))
        );
    }

    /// Неверное значение: ошибка проверки у поля; отказ Admin API — `detail` у названного в нём поля.
    #[test]
    fn invalid_value_shows_detail_at_field() {
        let c = catalog();
        let op = c.get("create_account").expect("create_account");
        let mut f = Form::new(op, &ctx(), false, &Map::new());
        assert!(f.request().is_none());
        assert_eq!(
            f.fields
                .iter()
                .find(|x| x.field.name == "login")
                .and_then(|x| x.error.as_deref()),
            Some("обязательное поле")
        );
        f.set("login", "не-адрес");
        assert!(f.request().is_some());
        f.apply_problem(&Problem {
            status: 400,
            title: "Bad Request".into(),
            detail: Some("login: не адрес в принятом домене".into()),
        });
        let login = f.fields.iter().position(|x| x.field.name == "login");
        assert_eq!(f.focus, login.expect("login"));
        assert_eq!(
            f.fields[f.focus].error.as_deref(),
            Some("login: не адрес в принятом домене")
        );
        assert_eq!(f.error, None);

        let list = c.get("list_accounts").expect("list_accounts");
        let mut q = Form::new(list, &ctx(), true, &Map::new());
        q.set("limit", "много");
        assert!(q.request().is_none());
        assert_eq!(
            q.fields
                .iter()
                .find(|x| x.field.name == "limit")
                .and_then(|x| x.error.as_deref()),
            Some("нужно целое число")
        );
        q.apply_problem(&Problem {
            status: 409,
            title: "Conflict".into(),
            detail: None,
        });
        assert_eq!(q.error.as_deref(), Some("409 Conflict"));
    }

    #[test]
    fn item_ops_of_accounts() {
        let c = catalog();
        let list = c.get("list_accounts").expect("list_accounts");
        let ops = item_ops(c, list);
        assert_eq!(ops.param, Some("account"));
        assert_eq!(ops.view.map(|o| o.id.as_str()), Some("get_account"));
        assert_eq!(ops.edit.map(|o| o.id.as_str()), Some("patch_account"));
        assert_eq!(ops.create.map(|o| o.id.as_str()), Some("create_account"));
        assert_eq!(ops.password.map(|o| o.id.as_str()), Some("issue"));
        assert_eq!(ops.delete, None);
        let groups = item_ops(c, c.get("list_groups").expect("list_groups"));
        assert_eq!(groups.delete.map(|o| o.id.as_str()), Some("delete_group"));
    }

    #[test]
    fn pages_with_cursor() {
        let c = catalog();
        let audit = c.get("search_audit").expect("search_audit");
        let (rows, next) = page(
            audit,
            &serde_json::json!({"entries": [{"seq": 1}], "next": 7}),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(next, Some(("after".to_owned(), "7".to_owned())));
        let (_, last) = page(audit, &serde_json::json!({"entries": [], "next": null}));
        assert_eq!(last, None);
    }
}
