//! Ресурсы: `GET`, `HEAD`, `PUT`, `DELETE` (RFC 4918 §9.4–9.7, RFC 4791 §5.3.2, RFC 6352 §6.3.2), `MKCALENDAR`
//! (RFC 4791 §5.3.1) и расширенный `MKCOL` (RFC 5689).
//!
//! `PUT` — объект коллекции: тело не больше `max-resource-size` (меньшее из `dav.max_object_size` и лимита
//! хранилища), индекс — [`crate::index`]; условия `If-Match` (в том числе `*` и список) и `If-None-Match: *` —
//! 412 при несовпадении. Ответ — 201 (создан) или 204 (перезаписан) с `ETag`: тело хранится без изменений
//! (RFC 4791 §5.3.4). Предусловия: `valid-calendar-data` / `valid-address-data`, `supported-calendar-component`,
//! `no-uid-conflict` (с `href` ресурса с тем же UID), `max-resource-size`; нет коллекции — 409.
//!
//! `MKCALENDAR` и `MKCOL` — коллекция с именем из последнего сегмента URL: в доме календарей — календарь, в доме
//! адресных книг — адресная книга (`resourcetype` другого вида — 403 `valid-resourcetype`). Начальные свойства —
//! как в `PROPPATCH` и `supported-calendar-component-set`; неизвестные свойства пропускаются (клиенты Apple
//! передают свои), ошибка известного — 403 с `mkcalendar-response` / `mkcol-response`, коллекция не создаётся.
//! Имя занято — 405. `DELETE` коллекции удаляет её вместе с объектами.

use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use tp_contracts::dav::{
    Collection, CollectionId, CollectionKind, Component, DeleteOutcome, Precondition, PutOutcome,
};
use tp_contracts::{Error, Result};

use crate::index;
use crate::path::Target;
use crate::props::{self, content_type};
use crate::server::{Cx, not_allowed, precondition, read, status, xml_response};
use crate::xml::{self, CALDAV, CARDDAV, DAV, Element, HEADER, XMLNS};

fn etag_header(etag: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!("\"{etag}\"")).ok()
}

async fn by_name(cx: &Cx<'_>, kind: CollectionKind, name: &str) -> Result<Option<Collection>> {
    cx.store().collection_by_name(cx.scope, kind, name).await
}

/// `GET` и `HEAD` объекта.
pub(crate) async fn get(cx: &Cx<'_>, target: &Target, head: bool) -> Result<Response> {
    let Target::Object(kind, _, coll, name) = target else {
        return Ok(not_allowed());
    };
    let Some(c) = by_name(cx, *kind, coll).await? else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    let Some(o) = cx.store().get_object(cx.scope, c.id, name).await? else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    let len = o.body.len();
    let mut resp = if head {
        Response::new(Body::empty())
    } else {
        Response::new(Body::from(o.body))
    };
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(*kind)),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if let Some(e) = etag_header(&o.etag) {
        h.insert(header::ETAG, e);
    }
    Ok(resp)
}

fn max_size(kind: CollectionKind) -> &'static str {
    match kind {
        CollectionKind::Calendar => "<c:max-resource-size/>",
        CollectionKind::AddressBook => "<card:max-resource-size/>",
    }
}

fn valid_data(kind: CollectionKind) -> &'static str {
    match kind {
        CollectionKind::Calendar => "<c:valid-calendar-data/>",
        CollectionKind::AddressBook => "<card:valid-address-data/>",
    }
}

/// Условие записи и удаления по `If-Match` и `If-None-Match`; `None` — условие уже не выполнено (412).
async fn condition(cx: &Cx<'_>, coll: CollectionId, name: &str) -> Result<Option<Precondition>> {
    let tags = |s: &str| -> Vec<String> {
        s.split(',')
            .map(|t| {
                t.trim()
                    .trim_start_matches("W/")
                    .trim_matches('"')
                    .to_owned()
            })
            .filter(|t| !t.is_empty())
            .collect()
    };
    if let Some(m) = cx.header("if-match") {
        let list = tags(m);
        if let [one] = list.as_slice()
            && one.as_str() != "*"
        {
            return Ok(Some(Precondition::IfMatch(one.clone())));
        }
        let cur = cx.store().get_object(cx.scope, coll, name).await?;
        return Ok(cur
            .filter(|o| list.iter().any(|t| t.as_str() == "*" || *t == o.etag))
            .map(|o| Precondition::IfMatch(o.etag)));
    }
    if let Some(n) = cx.header("if-none-match") {
        let list = tags(n);
        if list.iter().any(|t| t.as_str() == "*") {
            return Ok(Some(Precondition::IfNoneMatch));
        }
        let cur = cx.store().get_object(cx.scope, coll, name).await?;
        return Ok(match cur {
            Some(o) if list.contains(&o.etag) => None,
            Some(o) => Some(Precondition::IfMatch(o.etag)),
            None => Some(Precondition::IfNoneMatch),
        });
    }
    Ok(Some(Precondition::Any))
}

/// `PUT` объекта.
pub(crate) async fn put(cx: &Cx<'_>, target: &Target, body: Body) -> Result<Response> {
    let Target::Object(kind, _, coll, name) = target else {
        return Ok(not_allowed());
    };
    let kind = *kind;
    let Ok(bytes) = read(body, cx.dav.max_object_size()).await else {
        return Ok(precondition(StatusCode::FORBIDDEN, max_size(kind)));
    };
    let Some(c) = by_name(cx, kind, coll).await? else {
        return Ok(status(StatusCode::CONFLICT));
    };
    let Ok(text) = String::from_utf8(bytes.to_vec()) else {
        return Ok(precondition(StatusCode::FORBIDDEN, valid_data(kind)));
    };
    let idx = match index::index(&text, kind, &cx.dav.index_options()) {
        Ok(idx) => idx,
        Err(e) => {
            tracing::debug!(error = %e, "DAV PUT: тело не принято");
            return Ok(precondition(StatusCode::FORBIDDEN, valid_data(kind)));
        }
    };
    if !c.components.contains(&idx.component) {
        return Ok(precondition(
            StatusCode::FORBIDDEN,
            "<c:supported-calendar-component/>",
        ));
    }
    let Some(pre) = condition(cx, c.id, name).await? else {
        return Ok(status(StatusCode::PRECONDITION_FAILED));
    };
    match cx
        .store()
        .put_object(cx.scope, c.id, name, text, idx, pre)
        .await?
    {
        PutOutcome::Stored(put) => {
            let code = if put.created {
                StatusCode::CREATED
            } else {
                StatusCode::NO_CONTENT
            };
            let mut resp = status(code);
            if let Some(e) = etag_header(&put.etag) {
                resp.headers_mut().insert(header::ETAG, e);
            }
            Ok(resp)
        }
        PutOutcome::PreconditionFailed => Ok(status(StatusCode::PRECONDITION_FAILED)),
        PutOutcome::UidConflict { name: other } => {
            let p = match kind {
                CollectionKind::Calendar => "c",
                CollectionKind::AddressBook => "card",
            };
            let h = xml::href(&crate::path::object(kind, cx.login(), &c.name, &other));
            Ok(precondition(
                StatusCode::FORBIDDEN,
                &format!("<{p}:no-uid-conflict>{h}</{p}:no-uid-conflict>"),
            ))
        }
    }
}

/// `DELETE` объекта или коллекции.
pub(crate) async fn delete(cx: &Cx<'_>, target: &Target) -> Result<Response> {
    match target {
        Target::Object(kind, _, coll, name) => {
            let Some(c) = by_name(cx, *kind, coll).await? else {
                return Ok(status(StatusCode::NOT_FOUND));
            };
            let Some(pre) = condition(cx, c.id, name).await? else {
                return Ok(status(StatusCode::PRECONDITION_FAILED));
            };
            Ok(
                match cx.store().delete_object(cx.scope, c.id, name, pre).await? {
                    DeleteOutcome::Deleted(_) => status(StatusCode::NO_CONTENT),
                    DeleteOutcome::PreconditionFailed => status(StatusCode::PRECONDITION_FAILED),
                },
            )
        }
        Target::Collection(kind, _, name) => {
            let Some(c) = by_name(cx, *kind, name).await? else {
                return Ok(status(StatusCode::NOT_FOUND));
            };
            cx.store().delete_collection(cx.scope, c.id).await?;
            Ok(status(StatusCode::NO_CONTENT))
        }
        _ => Ok(status(StatusCode::FORBIDDEN)),
    }
}

/// `resourcetype` нового ресурса подходит дому вида `kind`.
fn resourcetype_fits(p: &Element, kind: CollectionKind) -> bool {
    let has = |ns: &str, n: &str| p.child(ns, n).is_some();
    has(DAV, "collection")
        && match kind {
            CollectionKind::Calendar => has(CALDAV, "calendar") && !has(CARDDAV, "addressbook"),
            CollectionKind::AddressBook => has(CARDDAV, "addressbook") && !has(CALDAV, "calendar"),
        }
}

/// `supported-calendar-component-set` новой коллекции.
fn components(c: &mut Collection, p: &Element) -> StatusCode {
    let mut set = BTreeSet::new();
    for comp in p.children.iter().filter(|x| x.is(CALDAV, "comp")) {
        match comp.attr("name").map(str::parse::<Component>) {
            Some(Ok(x)) if x.fits(c.kind) => {
                set.insert(x);
            }
            _ => return StatusCode::FORBIDDEN,
        }
    }
    if set.is_empty() {
        return StatusCode::FORBIDDEN;
    }
    c.components = set;
    StatusCode::OK
}

/// `MKCALENDAR` (`calendar`) и `MKCOL`.
pub(crate) async fn mkcol(
    cx: &Cx<'_>,
    target: &Target,
    body: Body,
    calendar: bool,
) -> Result<Response> {
    let Target::Collection(kind, _, name) = target else {
        return Ok(not_allowed());
    };
    let kind = *kind;
    if calendar && kind != CollectionKind::Calendar {
        return Ok(status(StatusCode::FORBIDDEN));
    }
    let el = match cx.xml(body).await {
        Ok(el) => el,
        Err(resp) => return Ok(resp),
    };
    let set: Vec<&Element> = match &el {
        None => Vec::new(),
        Some(root) => {
            let fits = if calendar {
                root.is(CALDAV, "mkcalendar")
            } else {
                root.is(DAV, "mkcol")
            };
            if !fits {
                return Ok(status(if calendar {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::UNSUPPORTED_MEDIA_TYPE
                }));
            }
            root.children
                .iter()
                .filter(|s| s.is(DAV, "set"))
                .flat_map(|s| s.children.iter().filter(|p| p.is(DAV, "prop")))
                .flat_map(|p| p.children.iter())
                .collect()
        }
    };
    if by_name(cx, kind, name).await?.is_some() {
        return Ok(not_allowed());
    }
    let mut c = Collection::new(kind, name.clone());
    let mut results = Vec::new();
    for p in set {
        let key = p.key();
        let st = if key.is(DAV, "resourcetype") {
            if !resourcetype_fits(p, kind) {
                return Ok(precondition(
                    StatusCode::FORBIDDEN,
                    "<d:valid-resourcetype/>",
                ));
            }
            StatusCode::OK
        } else if key.is(CALDAV, "supported-calendar-component-set")
            && kind == CollectionKind::Calendar
        {
            components(&mut c, p)
        } else {
            match props::apply(&mut c, &key, Some(p)) {
                StatusCode::FORBIDDEN => continue,
                s => s,
            }
        };
        results.push((key, st));
    }
    if results.iter().any(|(_, s)| *s != StatusCode::OK) {
        for (_, s) in &mut results {
            if *s == StatusCode::OK {
                *s = StatusCode::FAILED_DEPENDENCY;
            }
        }
        let root = if calendar {
            "c:mkcalendar-response"
        } else {
            "d:mkcol-response"
        };
        let mut out = format!("{HEADER}<{root}{XMLNS}>");
        xml::write_propstats(&mut out, &props::status_groups(results));
        out.push_str("</");
        out.push_str(root);
        out.push('>');
        return Ok(xml_response(StatusCode::FORBIDDEN, out));
    }
    match cx.store().create_collection(cx.scope, c).await {
        Ok(_) => Ok(status(StatusCode::CREATED)),
        Err(Error::Conflict) => Ok(not_allowed()),
        Err(e) => Err(e),
    }
}
