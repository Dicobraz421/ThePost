//! Отчёты по принципалам (DAV-18, DAV-19): поиск учётных записей и ресурсов для выбора участника и переговорной в
//! клиенте, раскрытие свойств-ссылок.
//!
//! - `principal-property-search` (RFC 3744 §9.4): условия `property-search` по `displayname` (и `cs:first-name`,
//!   `cs:last-name`), адресам (`calendar-user-address-set`, `cs:email-address-set`) — подстрока без учёта регистра, и по
//!   `calendar-user-type` — точное значение (`INDIVIDUAL`, `ROOM`); `test="anyof"` — любое условие, иначе все;
//! - `calendarserver-principal-search` (Календарь macOS и iOS): все `search-token` — подстроки имени или адреса;
//!   `context="location"` и `"resource"` — только ресурсы, `"attendee"` и `"user"` — только пользователи; `nresults`;
//! - `principal-search-property-set` — свойства, по которым ищется;
//! - `expand-property` (RFC 3253 §3.8) у принципала и группы представителей: `calendar-proxy-read-for`,
//!   `calendar-proxy-write-for` и `group-member-set` — вложенными `response` принципалов с запрошенными свойствами,
//!   прочие свойства — значениями.
//!
//! Ищутся включённые учётные записи организации вошедшего (`AccountStore::list` порциями `dav.gal_page_size`), не больше
//! `dav.principal_search_scan` записей; найденных — не больше `dav.principal_search_results`, остальное — `507` с
//! `number-of-matches-within-limits`. Без планирования учётных записей нет — пустой ответ.

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use tp_contracts::Result;
use tp_contracts::meta::{Account, AccountKind, AccountStore};

use crate::path::{self, Target};
use crate::props::{self, PrincipalView, Res, Want};
use crate::server::{Cx, multistatus, precondition, status};
use crate::xml::{self, CALDAV, CS, DAV, Element, Multistatus, PropName};

/// Поле условия поиска.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    /// Отображаемое имя.
    Name,
    /// Адрес (основной или псевдоним) или имя входа.
    Address,
    /// `calendar-user-type`.
    Type,
    /// Имя или адрес.
    Any,
    /// Свойство, по которому не ищется: условие не выполняется.
    Unknown,
}

/// Запрос поиска.
#[derive(Debug)]
struct Query {
    /// Условия: поле и значение (в нижнем регистре).
    tests: Vec<(Field, String)>,
    /// Все условия (иначе любое).
    all: bool,
    /// Вид учётных записей; `None` — любой.
    kind: Option<AccountKind>,
    /// Найденных в ответе.
    limit: usize,
    /// Свойства найденных.
    want: Want,
}

fn field(n: &PropName) -> Field {
    match (n.ns.as_str(), n.name.as_str()) {
        (DAV, "displayname") | (CS, "first-name" | "last-name") => Field::Name,
        (CALDAV, "calendar-user-address-set") | (CS, "email-address-set") => Field::Address,
        (CALDAV, "calendar-user-type") => Field::Type,
        _ => Field::Unknown,
    }
}

/// Свойства найденных: `prop` запроса, без него — имя и адреса.
fn want_of(root: &Element) -> Want {
    match root.child(DAV, "prop") {
        Some(p) => Want::Props(props::keys(p)),
        None => Want::Props(vec![
            PropName::new(DAV, "displayname"),
            PropName::new(CALDAV, "calendar-user-address-set"),
            PropName::new(CALDAV, "calendar-user-type"),
        ]),
    }
}

impl Query {
    /// `principal-property-search`; без условий — `None`.
    fn parse_dav(root: &Element, max: usize) -> Option<Query> {
        let mut tests = Vec::new();
        for s in root
            .children
            .iter()
            .filter(|c| c.is(DAV, "property-search"))
        {
            let value = s.child(DAV, "match")?.trimmed().to_lowercase();
            let names = props::keys(s.child(DAV, "prop")?);
            for n in names {
                tests.push((field(&n), value.clone()));
            }
        }
        if tests.is_empty() {
            return None;
        }
        Some(Query {
            tests,
            all: root.attr("test") != Some("anyof"),
            kind: None,
            limit: max,
            want: want_of(root),
        })
    }

    /// `calendarserver-principal-search`; без `search-token` — `None`.
    fn parse_cs(root: &Element, max: usize) -> Option<Query> {
        let tests: Vec<(Field, String)> = root
            .children
            .iter()
            .filter(|c| c.is(CS, "search-token"))
            .map(|t| (Field::Any, t.trimmed().to_lowercase()))
            .filter(|(_, v)| !v.is_empty())
            .collect();
        if tests.is_empty() {
            return None;
        }
        let kind = match root.attr("context") {
            Some("location" | "resource") => Some(AccountKind::Resource),
            Some("attendee" | "user") => Some(AccountKind::User),
            _ => None,
        };
        let limit = root
            .child(CS, "limit")
            .and_then(|l| l.child(CS, "nresults"))
            .and_then(|n| n.trimmed().parse::<usize>().ok())
            .map_or(max, |n| n.min(max));
        Some(Query {
            tests,
            all: true,
            kind,
            limit,
            want: want_of(root),
        })
    }

    fn test(field: Field, value: &str, a: &Account) -> bool {
        let value = value.strip_prefix("mailto:").unwrap_or(value);
        let name = || a.display_name.to_lowercase().contains(value);
        let address = || {
            a.login.to_lowercase().contains(value)
                || a.addresses().iter().any(|x| x.contains(value))
        };
        match field {
            Field::Name => name(),
            Field::Address => address(),
            Field::Any => name() || address(),
            Field::Type => props::cutype(a.kind).eq_ignore_ascii_case(value),
            Field::Unknown => false,
        }
    }

    fn matches(&self, a: &Account) -> bool {
        if !a.enabled || self.kind.is_some_and(|k| k != a.kind) {
            return false;
        }
        let mut it = self.tests.iter().map(|(f, v)| Self::test(*f, v, a));
        if self.all {
            it.all(|x| x)
        } else {
            it.any(|x| x)
        }
    }
}

/// Учётные записи организации (планирование).
fn accounts<'a>(cx: &'a Cx<'_>) -> Option<&'a dyn AccountStore> {
    cx.dav.accounts()
}

/// Найденные учётные записи (не больше `q.limit`) и признак, что найдено больше.
async fn search(cx: &Cx<'_>, q: &Query) -> Result<(Vec<Account>, bool)> {
    let Some(store) = accounts(cx) else {
        return Ok((Vec::new(), false));
    };
    let scan = cx.dav.cfg.principal_search_scan;
    let page = cx.dav.cfg.gal_page_size.max(1);
    let mut after: Option<String> = None;
    let mut seen = 0usize;
    let mut found = Vec::new();
    while seen < scan {
        let n = page.min(scan - seen);
        let list = store.list(cx.scope.tenant, "", after.as_deref(), n).await?;
        seen += list.len();
        let last = list.last().map(|a| a.login.to_lowercase());
        let full = list.len() == n;
        for a in list {
            if q.matches(&a) {
                if found.len() == q.limit {
                    return Ok((found, true));
                }
                found.push(a);
            }
        }
        match last {
            Some(l) if full => after = Some(l),
            _ => break,
        }
    }
    Ok((found, false))
}

/// `REPORT` у коллекции принципалов, принципала и группы представителей.
pub(crate) async fn report(cx: &Cx<'_>, target: &Target, body: Body) -> Result<Response> {
    let el = match cx.xml(body).await {
        Ok(Some(el)) => el,
        Ok(None) => return Ok(status(StatusCode::BAD_REQUEST)),
        Err(code) => return Ok(status(code)),
    };
    let max = cx.dav.cfg.principal_search_results;
    let query = match (el.ns.as_str(), el.name.as_str()) {
        (DAV, "principal-property-search") => Query::parse_dav(&el, max),
        (CS, "calendarserver-principal-search") => Query::parse_cs(&el, max),
        (DAV, "principal-search-property-set") => return Ok(property_set()),
        (DAV, "expand-property") => return expand(cx, target, &el).await,
        _ => {
            return Ok(precondition(StatusCode::FORBIDDEN, "<d:supported-report/>"));
        }
    };
    let Some(q) = query else {
        return Ok(status(StatusCode::BAD_REQUEST));
    };
    let (found, more) = search(cx, &q).await?;
    let mut ms = Multistatus::new();
    for a in &found {
        let view = PrincipalView::of(a);
        props::emit(cx, &mut ms, &Res::Principal(&view), &q.want);
    }
    if more {
        props::truncated(&mut ms, &target.href(cx.login()));
    }
    Ok(multistatus(ms))
}

/// `principal-search-property-set`.
fn property_set() -> Response {
    let mut out = format!(
        "{}<d:principal-search-property-set{}>",
        xml::HEADER,
        xml::XMLNS
    );
    for (prop, desc) in [
        ("<d:displayname/>", "Имя"),
        ("<c:calendar-user-address-set/>", "Адрес"),
        ("<c:calendar-user-type/>", "Вид"),
    ] {
        out.push_str(&format!(
            "<d:principal-search-property><d:prop>{prop}</d:prop>\
             <d:description xml:lang=\"ru\">{desc}</d:description></d:principal-search-property>"
        ));
    }
    out.push_str("</d:principal-search-property-set>");
    crate::server::xml_response(StatusCode::OK, out)
}

/// Свойство-ссылка на принципалов, раскрываемое `expand-property`.
fn expandable(n: &PropName) -> bool {
    n.is(CS, "calendar-proxy-read-for")
        || n.is(CS, "calendar-proxy-write-for")
        || n.is(DAV, "group-member-set")
}

/// `expand-property`: свойства ресурса запроса; ссылки на принципалов — вложенными `response`.
async fn expand(cx: &Cx<'_>, target: &Target, root: &Element) -> Result<Response> {
    let (view, members) = match target {
        Target::Principal(_) => (Some(props::principal_view(cx).await?), Vec::new()),
        Target::Proxy(_, write) => (None, crate::proxy::members(cx, *write).await?),
        _ => {
            return Ok(precondition(StatusCode::FORBIDDEN, "<d:supported-report/>"));
        }
    };
    let res = match (&view, target) {
        (Some(v), _) => Res::Principal(v),
        (None, Target::Proxy(_, write)) => Res::Proxy {
            owner: cx.login(),
            write: *write,
            members: &members,
        },
        _ => return Ok(status(StatusCode::BAD_REQUEST)),
    };
    let requested: Vec<(PropName, &Element)> = root
        .children
        .iter()
        .filter(|c| c.is(DAV, "property"))
        .filter_map(|p| Some((prop_name(p)?, p)))
        .collect();
    let plain: Vec<PropName> = requested.iter().map(|(n, _)| n.clone()).collect();
    let mut groups = props::groups(cx, &res, &Want::Props(plain));
    for (name, el) in &requested {
        let inner: Vec<PropName> = el
            .children
            .iter()
            .filter(|c| c.is(DAV, "property"))
            .filter_map(prop_name)
            .collect();
        if !expandable(name) || inner.is_empty() {
            continue;
        }
        let logins: Vec<String> = match name.name.as_str() {
            "group-member-set" => members.clone(),
            _ => {
                let write = name.name == "calendar-proxy-write-for";
                view.as_ref()
                    .and_then(|v| v.proxy_for.as_ref())
                    .map(|l| {
                        l.iter()
                            .filter(|(_, w)| *w == write)
                            .map(|(o, _)| o.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            }
        };
        let mut value = String::new();
        let want = Want::Props(inner);
        if let Some(store) = accounts(cx) {
            for login in &logins {
                let Some(a) = store.by_login(cx.scope.tenant, login).await? else {
                    continue;
                };
                let v = PrincipalView::of(&a);
                value.push_str("<d:response>");
                value.push_str(&xml::href(&path::principal(&v.login)));
                xml::write_propstats(&mut value, &props::groups(cx, &Res::Principal(&v), &want));
                value.push_str("</d:response>");
            }
        }
        for (_, list) in &mut groups {
            for (n, v) in list.iter_mut() {
                if n == name {
                    *v = value.clone();
                }
            }
        }
    }
    let mut ms = Multistatus::new();
    ms.props(&target.href(cx.login()), &groups);
    Ok(multistatus(ms))
}

/// Имя свойства элемента `property` (`name`, `namespace` — по умолчанию `DAV:`).
fn prop_name(p: &Element) -> Option<PropName> {
    let name = p.attr("name")?;
    Some(PropName::new(p.attr("namespace").unwrap_or(DAV), name))
}
