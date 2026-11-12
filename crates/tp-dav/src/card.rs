//! Отчёты CardDAV по адресной книге: `addressbook-query` (RFC 6352 §8.6) и `addressbook-multiget` (§8.7).
//!
//! `addressbook-query`: фильтр `filter` → `prop-filter` → `param-filter` с `is-not-defined` и `text-match`
//! (`match-type`: `equals`, `contains` — по умолчанию, `starts-with`, `ends-with`; сравнения `i;unicode-casemap` —
//! по умолчанию, `i;ascii-casemap`, `i;octet`; `negate-condition`); `test="anyof|allof"` у `filter` и `prop-filter`
//! (по умолчанию `anyof`). Условия `prop-filter` проверяются на каждом свойстве с этим именем (группа в имени не
//! учитывается): карточка подходит, если подходит хотя бы одно. Значение свойства для сравнения — каждая его часть
//! и все части через `;` (`N`, `ADR`). Фильтр без условий и запрос без `filter` — все карточки. Неверный фильтр —
//! 400, неизвестное сравнение — 403 `supported-collation`.
//!
//! `limit` (`nresults`, не больше `dav.max_response_items`): остаток — `507` с `number-of-matches-within-limits`
//! для адресной книги (§8.6.2). `address-data` с `prop` — карточка только с названными свойствами (`VERSION` — всегда).

use std::borrow::Cow;
use std::collections::HashMap;

use axum::http::StatusCode;
use axum::response::Response;
use calcard::vcard::{VCard, VCardEntry, VCardValue, VCardVersion};
use tp_contracts::Result;
use tp_contracts::dav::{Collection, CollectionKind, DavObject, TimeRange};

use crate::path::{self, Target};
use crate::props::{self, Res, Want};
use crate::query::{FilterError, TextMatch, object_name, unsupported, want_of};
use crate::server::{Cx, multistatus, precondition, status};
use crate::xml::{CARDDAV, DAV, Element, Multistatus};

type Parsed<T> = std::result::Result<T, FilterError>;

/// `param-filter`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParamFilter {
    name: String,
    not_defined: bool,
    text: Option<TextMatch>,
}

/// `prop-filter`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PropFilter {
    name: String,
    not_defined: bool,
    /// `test="allof"`.
    all: bool,
    texts: Vec<TextMatch>,
    params: Vec<ParamFilter>,
}

/// `filter`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Filter {
    /// `test="allof"`.
    all: bool,
    props: Vec<PropFilter>,
}

fn name_of(el: &Element) -> Parsed<String> {
    el.attr("name")
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .ok_or(FilterError::Invalid)
}

/// Атрибут `test`: `allof` — `true`.
fn all_of(el: &Element) -> Parsed<bool> {
    match el.attr("test") {
        None | Some("anyof") => Ok(false),
        Some("allof") => Ok(true),
        Some(_) => Err(FilterError::Invalid),
    }
}

fn text_matches(el: &Element) -> Parsed<Vec<TextMatch>> {
    el.children
        .iter()
        .filter(|c| c.is(CARDDAV, "text-match"))
        .map(TextMatch::parse_card)
        .collect()
}

impl ParamFilter {
    fn parse(el: &Element) -> Parsed<ParamFilter> {
        Ok(ParamFilter {
            name: name_of(el)?,
            not_defined: el.child(CARDDAV, "is-not-defined").is_some(),
            text: text_matches(el)?.into_iter().next(),
        })
    }

    fn matches(&self, e: &VCardEntry) -> bool {
        let mut params = e
            .params
            .iter()
            .filter(|p| p.name.as_str().eq_ignore_ascii_case(&self.name))
            .peekable();
        if self.not_defined {
            return params.peek().is_none();
        }
        params.any(|p| {
            self.text
                .as_ref()
                .is_none_or(|t| t.matches([p.value.clone().into_text().as_ref()]))
        })
    }
}

impl PropFilter {
    fn parse(el: &Element) -> Parsed<PropFilter> {
        Ok(PropFilter {
            name: name_of(el)?,
            not_defined: el.child(CARDDAV, "is-not-defined").is_some(),
            all: all_of(el)?,
            texts: text_matches(el)?,
            params: el
                .children
                .iter()
                .filter(|c| c.is(CARDDAV, "param-filter"))
                .map(ParamFilter::parse)
                .collect::<Parsed<_>>()?,
        })
    }

    fn entry_matches(&self, e: &VCardEntry) -> bool {
        let texts = entry_texts(e);
        let mut conds = self
            .texts
            .iter()
            .map(|t| t.matches(texts.iter().map(Cow::as_ref)))
            .chain(self.params.iter().map(|p| p.matches(e)))
            .peekable();
        if conds.peek().is_none() {
            return true;
        }
        if self.all {
            conds.all(|x| x)
        } else {
            conds.any(|x| x)
        }
    }

    fn matches(&self, card: &VCard) -> bool {
        let mut entries = card
            .entries
            .iter()
            .filter(|e| e.name.as_str().eq_ignore_ascii_case(&self.name))
            .peekable();
        if self.not_defined {
            return entries.peek().is_none();
        }
        entries.any(|e| self.entry_matches(e))
    }
}

impl Filter {
    pub(crate) fn parse(el: &Element) -> Parsed<Filter> {
        Ok(Filter {
            all: all_of(el)?,
            props: el
                .children
                .iter()
                .filter(|c| c.is(CARDDAV, "prop-filter"))
                .map(PropFilter::parse)
                .collect::<Parsed<_>>()?,
        })
    }

    pub(crate) fn matches(&self, card: &VCard) -> bool {
        if self.props.is_empty() {
            return true;
        }
        if self.all {
            self.props.iter().all(|p| p.matches(card))
        } else {
            self.props.iter().any(|p| p.matches(card))
        }
    }
}

/// Значения свойства для сравнения: каждая часть и, если частей несколько, все через `;`.
fn entry_texts(e: &VCardEntry) -> Vec<Cow<'_, str>> {
    let mut parts: Vec<Cow<'_, str>> = Vec::new();
    for v in &e.values {
        match v {
            VCardValue::Component(items) => {
                parts.extend(items.iter().map(|s| Cow::Borrowed(s.as_str())));
            }
            VCardValue::Integer(i) => parts.push(Cow::Owned(i.to_string())),
            VCardValue::Float(f) => parts.push(Cow::Owned(f.to_string())),
            other => parts.extend(other.as_text().map(Cow::Borrowed)),
        }
    }
    if parts.len() > 1 {
        parts.push(Cow::Owned(parts.join(";")));
    }
    parts
}

/// Параметры `address-data` запроса.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CardData {
    /// Запрошенные свойства (`prop name`); пусто — карточка целиком.
    props: Vec<String>,
}

impl CardData {
    /// Из элемента `prop` запроса (с `address-data` или без).
    pub(crate) fn parse(prop: Option<&Element>) -> CardData {
        let props = prop
            .and_then(|p| p.child(CARDDAV, "address-data"))
            .map(|d| {
                d.children
                    .iter()
                    .filter(|c| c.is(CARDDAV, "prop"))
                    .filter_map(|c| c.attr("name"))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        CardData { props }
    }

    /// Тело возвращается как хранится.
    pub(crate) fn is_plain(&self) -> bool {
        self.props.is_empty()
    }

    /// Тело карточки по параметрам запроса; тело, которое не разбирается, — как есть.
    pub(crate) fn render<'b>(&self, body: &'b str, card: Option<&VCard>) -> Cow<'b, str> {
        if self.is_plain() {
            return Cow::Borrowed(body);
        }
        let parsed;
        let card = match card {
            Some(c) => c,
            None => match VCard::parse(body) {
                Ok(c) => {
                    parsed = c;
                    &parsed
                }
                Err(_) => return Cow::Borrowed(body),
            },
        };
        let version = card.version().unwrap_or(VCardVersion::V3_0);
        let out = VCard {
            entries: card
                .entries
                .iter()
                .filter(|e| {
                    self.props
                        .iter()
                        .any(|p| p.eq_ignore_ascii_case(e.name.as_str()))
                })
                .cloned()
                .collect(),
        };
        let mut text = String::with_capacity(body.len());
        match out.write_to(&mut text, version) {
            Ok(()) => Cow::Owned(text),
            Err(_) => Cow::Borrowed(body),
        }
    }
}

/// Отчёты CardDAV (корневой элемент `root` в пространстве CardDAV).
pub(crate) async fn report(cx: &Cx<'_>, target: &Target, root: &Element) -> Result<Response> {
    let Target::Collection(CollectionKind::AddressBook, _, name) = target else {
        return Ok(unsupported());
    };
    let kind = CollectionKind::AddressBook;
    let Some(c) = cx.store().collection_by_name(cx.scope, kind, name).await? else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    match root.name.as_str() {
        "addressbook-query" => {
            let objects = cx
                .store()
                .in_range(cx.scope, c.id, TimeRange::default())
                .await?;
            Ok(query(cx, &c, &objects, root))
        }
        "addressbook-multiget" => multiget(cx, &c, root).await,
        _ => Ok(unsupported()),
    }
}

fn emit(
    cx: &Cx<'_>,
    ms: &mut Multistatus,
    c: &Collection,
    o: &DavObject,
    want: &Want,
    body: Option<&str>,
) {
    let res = Res::Object {
        coll: c,
        name: &o.name,
        etag: &o.etag,
        body,
    };
    props::emit(cx, ms, &res, want);
}

/// `addressbook-query` по карточкам `objects` книги `c`.
fn query(cx: &Cx<'_>, c: &Collection, objects: &[DavObject], root: &Element) -> Response {
    let filter = match root.child(CARDDAV, "filter").map(Filter::parse) {
        None => Filter::default(),
        Some(Ok(f)) => f,
        Some(Err(FilterError::Collation)) => {
            return precondition(StatusCode::FORBIDDEN, "<card:supported-collation/>");
        }
        Some(Err(FilterError::Unsupported)) => {
            return precondition(StatusCode::FORBIDDEN, "<card:supported-filter/>");
        }
        Some(Err(FilterError::Invalid)) => return status(StatusCode::BAD_REQUEST),
    };
    let nresults = root
        .child(CARDDAV, "limit")
        .and_then(|l| l.child(CARDDAV, "nresults"))
        .map(|n| n.trimmed().parse::<usize>());
    let max = match nresults {
        None => cx.dav.cfg.max_response_items,
        Some(Ok(n)) => n.min(cx.dav.cfg.max_response_items),
        Some(Err(_)) => return status(StatusCode::BAD_REQUEST),
    };
    let data = CardData::parse(root.child(DAV, "prop"));
    let want = want_of(root);
    let need_body = want.needs_body();
    let mut ms = Multistatus::new();
    let mut sent = 0usize;
    let mut more = false;
    for o in objects {
        let Ok(card) = VCard::parse(&o.body) else {
            continue;
        };
        if !filter.matches(&card) {
            continue;
        }
        if sent == max {
            more = true;
            break;
        }
        sent += 1;
        let body = need_body.then(|| data.render(&o.body, Some(&card)));
        emit(cx, &mut ms, c, o, &want, body.as_deref());
    }
    if more {
        props::truncated(&mut ms, &path::collection(c.kind, cx.login(), &c.name));
    }
    multistatus(ms)
}

/// `addressbook-multiget`: карточки по ссылкам в порядке запроса; отсутствующие и чужие — 404 в ответе.
async fn multiget(cx: &Cx<'_>, c: &Collection, root: &Element) -> Result<Response> {
    let hrefs: Vec<&str> = root
        .children
        .iter()
        .filter(|e| e.is(DAV, "href"))
        .map(Element::trimmed)
        .collect();
    if hrefs.len() > cx.dav.cfg.max_multiget_hrefs {
        return Ok(precondition(
            StatusCode::FORBIDDEN,
            "<d:number-of-matches-within-limits/>",
        ));
    }
    let data = CardData::parse(root.child(DAV, "prop"));
    let want = want_of(root);
    let need_body = want.needs_body();
    let resolved: Vec<(&str, Option<String>)> = hrefs
        .into_iter()
        .map(|h| (h, object_name(cx, c, h)))
        .collect();
    let mut names: Vec<String> = resolved.iter().filter_map(|(_, n)| n.clone()).collect();
    names.sort_unstable();
    names.dedup();
    let objects = if names.is_empty() {
        Vec::new()
    } else {
        cx.store().multiget(cx.scope, c.id, &names).await?
    };
    let by_name: HashMap<&str, &DavObject> = objects.iter().map(|o| (o.name.as_str(), o)).collect();
    let mut ms = Multistatus::new();
    for (href, name) in &resolved {
        let Some(o) = name.as_deref().and_then(|n| by_name.get(n)) else {
            ms.status(href, StatusCode::NOT_FOUND, None);
            continue;
        };
        let body = need_body.then(|| data.render(&o.body, None));
        emit(cx, &mut ms, c, o, &want, body.as_deref());
    }
    Ok(multistatus(ms))
}
