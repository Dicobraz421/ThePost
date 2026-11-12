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
//!
//! Версия vCard: карточка хранится в принятой версии и так же выдаётся, пока клиент не назвал другую — `Accept:
//! text/vcard; version=3.0|4.0` у `GET` или атрибуты `content-type`, `version` у `address-data` отчётов (другая
//! версия или тип в `address-data` — 403 `supported-address-data`). Преобразование — запись calcard в нужной версии
//! ([`convert`]) и то, чего она не делает: `TYPE=pref` ↔ `PREF=1`, группы Apple `X-ADDRESSBOOKSERVER-KIND`,
//! `X-ADDRESSBOOKSERVER-MEMBER` (3.0) ↔ `KIND`, `MEMBER` (4.0); в 3.0 добавляется обязательное `N` (из `FN`), в 4.0
//! убираются `CHARSET` и тип встроенного фото из `TYPE` (он — в значении `data:`).
//!
//! Проверка карточки при `PUT` ([`check_put`], RFC 6352 §6.3.2.1): `valid-address-data` (не разбирается, нет `UID`),
//! `supported-address-data` (версия не 3.0 и не 4.0), `max-resource-size` — встроенное фото больше
//! `dav.max_photo_size` (публикуется как `max-image-size`).

use std::borrow::Cow;
use std::collections::HashMap;

use axum::http::StatusCode;
use axum::response::Response;
use calcard::vcard::{
    VCard, VCardEntry, VCardKind, VCardParameter, VCardParameterName, VCardParameterValue,
    VCardProperty, VCardValue, VCardVersion,
};
use tp_contracts::Result;
use tp_contracts::dav::{Collection, CollectionKind, DavObject, ObjectIndex, TimeRange};

use crate::gal;
use crate::index;

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

/// Свойство и свойство-участник группы Apple в vCard 3.0.
const APPLE_KIND: &str = "X-ADDRESSBOOKSERVER-KIND";
const APPLE_MEMBER: &str = "X-ADDRESSBOOKSERVER-MEMBER";

/// Версия vCard, в которой сервер выдаёт карточки: `3.0` или `4.0`.
fn version_of(s: &str) -> Option<VCardVersion> {
    match s.trim().trim_matches('"') {
        "3.0" => Some(VCardVersion::V3_0),
        "4.0" => Some(VCardVersion::V4_0),
        _ => None,
    }
}

/// Версия vCard из заголовка `Accept` (`text/vcard; version=4.0`); не названа или не поддерживается — `None`.
pub(crate) fn accept_version(accept: &str) -> Option<VCardVersion> {
    accept.split(',').find_map(|part| {
        let mut it = part.split(';').map(str::trim);
        if !it.next()?.eq_ignore_ascii_case("text/vcard") {
            return None;
        }
        it.filter_map(|p| p.split_once('='))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("version"))
            .and_then(|(_, v)| version_of(v))
    })
}

fn is_pref(p: &VCardParameter) -> bool {
    p.name == VCardParameterName::Type
        && matches!(&p.value, VCardParameterValue::Text(t) if t.eq_ignore_ascii_case("pref"))
}

/// Карточка, подготовленная к записи в версии `to` (то, чего не делает запись calcard, — см. описание модуля).
fn adapt(card: &VCard, to: VCardVersion) -> VCard {
    let v4 = to == VCardVersion::V4_0;
    let mut entries: Vec<VCardEntry> = card.entries.clone();
    for e in &mut entries {
        if v4 {
            e.params
                .retain(|p| !p.name.as_str().eq_ignore_ascii_case("CHARSET"));
            // Тип фото в 4.0 — в самом значении `data:` (в 3.0 запись calcard вернёт его в `TYPE`).
            let media = e
                .values
                .iter()
                .filter_map(VCardValue::as_binary)
                .find_map(|d| d.content_type.as_deref())
                .and_then(|t| t.rsplit('/').next());
            if let Some(sub) = media {
                e.params.retain(|p| {
                    p.name != VCardParameterName::Type
                        || !p
                            .value
                            .as_text()
                            .is_some_and(|t| t.eq_ignore_ascii_case(sub))
                });
            }
            for p in &mut e.params {
                if is_pref(p) {
                    *p = VCardParameter::pref(1u32);
                }
            }
            if e.name.as_str().eq_ignore_ascii_case(APPLE_KIND) {
                e.name = VCardProperty::Kind;
                for v in &mut e.values {
                    let kind = match v.as_text().map(str::to_ascii_lowercase).as_deref() {
                        Some("group") => VCardKind::Group,
                        Some("individual") => VCardKind::Individual,
                        Some("org") => VCardKind::Org,
                        Some("location") => VCardKind::Location,
                        _ => continue,
                    };
                    *v = VCardValue::Kind(kind);
                }
            } else if e.name.as_str().eq_ignore_ascii_case(APPLE_MEMBER) {
                e.name = VCardProperty::Member;
            }
        } else {
            for p in &mut e.params {
                if p.name == VCardParameterName::Pref && p.value == VCardParameterValue::Integer(1)
                {
                    *p = VCardParameter::typ("pref".to_owned());
                }
            }
            match e.name {
                VCardProperty::Kind => {
                    e.name = VCardProperty::Other(APPLE_KIND.to_owned());
                    for v in &mut e.values {
                        if let Some(t) = v.as_text().map(str::to_ascii_lowercase) {
                            *v = VCardValue::Text(t);
                        }
                    }
                }
                VCardProperty::Member => e.name = VCardProperty::Other(APPLE_MEMBER.to_owned()),
                _ => {}
            }
        }
    }
    if !v4
        && !entries.iter().any(|e| e.name == VCardProperty::N)
        && let Some(name) = card
            .property(&VCardProperty::Fn)
            .and_then(|e| e.values.first())
            .and_then(VCardValue::as_text)
    {
        let mut n = VCardEntry::new(VCardProperty::N).with_value(name.to_owned());
        n.values
            .extend((0..4).map(|_| VCardValue::Text(String::new())));
        entries.push(n);
    }
    VCard { entries }
}

/// Запись карточки в версии `to`.
pub(crate) fn convert(card: &VCard, to: VCardVersion) -> Option<String> {
    let mut out = String::new();
    adapt(card, to).write_to(&mut out, to).ok()?;
    Some(out)
}

/// Тело карточки в версии `to`: уже в ней или не разбирается — как есть.
pub(crate) fn in_version(body: &str, to: VCardVersion) -> Cow<'_, str> {
    match VCard::parse(body) {
        Ok(card) if card.version() != Some(to) => {
            convert(&card, to).map_or(Cow::Borrowed(body), Cow::Owned)
        }
        _ => Cow::Borrowed(body),
    }
}

/// Проверка тела `PUT` в адресную книгу: индекс или нарушенное предусловие (см. описание модуля).
pub(crate) fn check_put(
    body: &str,
    max_photo_size: usize,
) -> std::result::Result<ObjectIndex, &'static str> {
    const VALID: &str = "<card:valid-address-data/>";
    let card = VCard::parse(body).map_err(|_| VALID)?;
    if !matches!(
        card.version(),
        Some(VCardVersion::V3_0 | VCardVersion::V4_0)
    ) {
        return Err("<card:supported-address-data/>");
    }
    let photo = card
        .properties(&VCardProperty::Photo)
        .flat_map(|e| &e.values)
        .filter_map(VCardValue::as_binary)
        .map(|d| d.data.len())
        .max();
    if photo.is_some_and(|len| len > max_photo_size) {
        return Err("<card:max-resource-size/>");
    }
    index::card(body).map_err(|_| VALID)
}

/// Параметры `address-data` запроса.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CardData {
    /// Запрошенная версия; `None` — как хранится.
    version: Option<VCardVersion>,
    /// Запрошенные свойства (`prop name`); пусто — карточка целиком.
    props: Vec<String>,
}

impl CardData {
    /// Из элемента `prop` запроса (с `address-data` или без); тип не `text/vcard` или версия не 3.0 и не 4.0 —
    /// `None` (`supported-address-data`).
    pub(crate) fn parse(prop: Option<&Element>) -> Option<CardData> {
        let Some(d) = prop.and_then(|p| p.child(CARDDAV, "address-data")) else {
            return Some(CardData::default());
        };
        if d.attr("content-type")
            .is_some_and(|t| !t.trim().eq_ignore_ascii_case("text/vcard"))
        {
            return None;
        }
        let version = match d.attr("version") {
            None => None,
            Some(v) => Some(version_of(v)?),
        };
        let props = d
            .children
            .iter()
            .filter(|c| c.is(CARDDAV, "prop"))
            .filter_map(|c| c.attr("name"))
            .map(str::to_owned)
            .collect();
        Some(CardData { version, props })
    }

    /// Тело карточки по параметрам запроса; тело, которое не разбирается, — как есть.
    pub(crate) fn render<'b>(&self, body: &'b str, card: Option<&VCard>) -> Cow<'b, str> {
        if self.props.is_empty() && self.version.is_none() {
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
        let stored = card.version();
        let version = self.version.or(stored).unwrap_or(VCardVersion::V3_0);
        if self.props.is_empty() && stored == Some(version) {
            return Cow::Borrowed(body);
        }
        let mut out = if stored == Some(version) {
            card.clone()
        } else {
            adapt(card, version)
        };
        if !self.props.is_empty() {
            out.entries.retain(|e| {
                self.props
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(e.name.as_str()))
            });
        }
        let mut text = String::with_capacity(body.len());
        match out.write_to(&mut text, version) {
            Ok(()) => Cow::Owned(text),
            Err(_) => Cow::Borrowed(body),
        }
    }
}

fn unsupported_data() -> Response {
    precondition(StatusCode::FORBIDDEN, "<card:supported-address-data/>")
}

/// Отчёты CardDAV (корневой элемент `root` в пространстве CardDAV).
pub(crate) async fn report(cx: &Cx<'_>, target: &Target, root: &Element) -> Result<Response> {
    let Target::Collection(CollectionKind::AddressBook, _, name) = target else {
        return Ok(unsupported());
    };
    let kind = CollectionKind::AddressBook;
    if let Some(g) = gal::find(cx, kind, name).await? {
        return match root.name.as_str() {
            "addressbook-query" => Ok(query(cx, &g.coll, &g.cards, root)),
            "addressbook-multiget" => multiget(cx, &g.coll, Some(&g.cards), root).await,
            _ => Ok(unsupported()),
        };
    }
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
        "addressbook-multiget" => multiget(cx, &c, None, root).await,
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
        tag: None,
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
    let Some(data) = CardData::parse(root.child(DAV, "prop")) else {
        return unsupported_data();
    };
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

/// `addressbook-multiget`: карточки по ссылкам в порядке запроса; отсутствующие и чужие — 404 в ответе. Карточки —
/// из `all` (глобальная адресная книга) или из хранилища.
async fn multiget(
    cx: &Cx<'_>,
    c: &Collection,
    all: Option<&[DavObject]>,
    root: &Element,
) -> Result<Response> {
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
    let Some(data) = CardData::parse(root.child(DAV, "prop")) else {
        return Ok(unsupported_data());
    };
    let want = want_of(root);
    let need_body = want.needs_body();
    let resolved: Vec<(&str, Option<String>)> = hrefs
        .into_iter()
        .map(|h| (h, object_name(cx, c, h)))
        .collect();
    let mut names: Vec<String> = resolved.iter().filter_map(|(_, n)| n.clone()).collect();
    names.sort_unstable();
    names.dedup();
    let stored = match all {
        Some(_) => Vec::new(),
        None if names.is_empty() => Vec::new(),
        None => cx.store().multiget(cx.scope, c.id, &names).await?,
    };
    let by_name: HashMap<&str, &DavObject> = all
        .unwrap_or(&stored)
        .iter()
        .map(|o| (o.name.as_str(), o))
        .collect();
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
