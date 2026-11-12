//! Свойства ресурсов: `PROPFIND` (RFC 4918 §9.1) и `PROPPATCH` (§9.2).
//!
//! `PROPFIND`: `Depth: 0` или `1` (`infinity` и отсутствие заголовка — 403 `propfind-finite-depth`), тело `prop`,
//! `allprop` (с `include`) или `propname`, пустое тело — `allprop`. Неизвестное свойство — 404 в `propstat`.
//! `allprop` возвращает `resourcetype`, `displayname`, `getetag`, `getcontenttype`; вычисляемые свойства
//! (`current-user-privilege-set`, `sync-token`, `supported-report-set` и др.) — только по имени. Список дочерних
//! ресурсов (`Depth: 1`) ограничен `dav.max_response_items`: остаток — `507` с `number-of-matches-within-limits`.
//!
//! Дом календарей: при первом `PROPFIND`, если календарей нет, создаются календарь (`calendar`, VEVENT) и список
//! задач (`tasks`, VTODO) с именами из `dav.default_calendar_name`, `dav.default_tasks_name`. Принципал публикует
//! `schedule-inbox-URL` и `schedule-outbox-URL` (`inbox/`, `outbox/` дома календарей — заготовки для DAV-13:
//! отвечают только на `PROPFIND`, имена зарезервированы).
//!
//! Дом адресных книг (`addressbook-home-set` принципала): при первом `PROPFIND`, если адресных книг нет, создаётся
//! книга `contacts` с именем из `dav.default_addressbook_name`. Свойства книги: `supported-address-data` (vCard 3.0
//! и 4.0), `max-resource-size`, `max-image-size` (Apple; `dav.max_photo_size`), `addressbook-description`.
//! Глобальная адресная книга ([`crate::gal`]) — последней в списке дома, права — только чтение.
//!
//! `PROPPATCH` — у коллекции: `displayname`, описание (`calendar-description`, `addressbook-description`), цвет и
//! порядок календаря (`calendar-color`, `calendar-order`, Apple), часовой пояс календаря (`calendar-timezone` —
//! VTIMEZONE, RFC 4791 §5.2.2; `calendar-timezone-id` — IANA, RFC 7809 §5.2; хранится идентификатор IANA). Изменения атомарны: ошибка одного свойства — оно
//! со своим статусом (403 — защищённое или неизвестное, 409 — недопустимое значение), остальные — 424, ничего не
//! записано. Произвольные («мёртвые») свойства не хранятся.

use std::collections::BTreeSet;
use std::str::FromStr;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use calcard::common::timezone::Tz;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarProperty,
};
use time::OffsetDateTime;
use tp_contracts::ModSeq;
use tp_contracts::dav::{Collection, CollectionKind, Component, DavStore};
use tp_contracts::meta::AccountScope;
use tp_contracts::{Error, Result};

use crate::config::DavConfig;
use crate::gal;
use crate::path::{self, Target};
use crate::server::{Cx, multistatus, precondition, status};
use crate::xml::{
    CALDAV, CARDDAV, CS, DAV, Element, Group, ICAL, Multistatus, PropName, href, text,
};

/// Префикс токена синхронизации (RFC 6578: токен — URI).
pub(crate) const SYNC_PREFIX: &str = "urn:thepost:sync:";

/// Токен синхронизации состояния `state`.
pub(crate) fn sync_token(state: ModSeq) -> String {
    format!("{SYNC_PREFIX}{}", state.0)
}

/// Свойства `allprop`.
const ALLPROP: [(&str, &str); 4] = [
    (DAV, "resourcetype"),
    (DAV, "displayname"),
    (DAV, "getetag"),
    (DAV, "getcontenttype"),
];

/// Права владельца на свои ресурсы (RFC 3744 §5.4).
const PRIV_OWNER: &str = concat!(
    "<d:privilege><d:all/></d:privilege><d:privilege><d:read/></d:privilege>",
    "<d:privilege><d:write/></d:privilege><d:privilege><d:write-properties/></d:privilege>",
    "<d:privilege><d:write-content/></d:privilege><d:privilege><d:bind/></d:privilege>",
    "<d:privilege><d:unbind/></d:privilege>",
    "<d:privilege><d:read-current-user-privilege-set/></d:privilege>"
);

/// Права на служебные ресурсы (корень, принципалы): только чтение.
const PRIV_READ: &str = concat!(
    "<d:privilege><d:read/></d:privilege>",
    "<d:privilege><d:read-current-user-privilege-set/></d:privilege>"
);

/// Ресурс, свойства которого возвращаются.
pub(crate) enum Res<'a> {
    Root,
    Principals,
    Principal,
    Home(CollectionKind),
    /// Ресурс планирования дома календарей: входящие (`true`) или исходящие.
    Schedule(bool),
    Collection(&'a Collection),
    Object {
        coll: &'a Collection,
        name: &'a str,
        etag: &'a str,
        /// Тело — если запрошены данные объекта.
        body: Option<&'a str>,
    },
}

impl Res<'_> {
    fn href(&self, login: &str) -> String {
        match self {
            Res::Root => path::PREFIX.to_owned(),
            Res::Principals => path::PRINCIPALS.to_owned(),
            Res::Principal => path::principal(login),
            Res::Home(k) => path::home(*k, login),
            Res::Schedule(inbox) => schedule_href(login, *inbox),
            Res::Collection(c) => path::collection(c.kind, login, &c.name),
            Res::Object { coll, name, .. } => path::object(coll.kind, login, &coll.name, name),
        }
    }
}

/// Ссылка на ресурс планирования: входящие (`true`) или исходящие.
fn schedule_href(login: &str, inbox: bool) -> String {
    let name = if inbox { path::INBOX } else { path::OUTBOX };
    path::collection(CollectionKind::Calendar, login, name)
}

/// Запрошенные свойства.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Want {
    /// `allprop` и свойства `include`.
    All(Vec<PropName>),
    /// `propname`.
    Names,
    /// `prop`.
    Props(Vec<PropName>),
}

impl Want {
    /// Тело `PROPFIND`; нет тела — `allprop`; неверное — `None`.
    pub(crate) fn parse(el: Option<&Element>) -> Option<Want> {
        let Some(root) = el else {
            return Some(Want::All(Vec::new()));
        };
        if !root.is(DAV, "propfind") {
            return None;
        }
        if let Some(p) = root.child(DAV, "prop") {
            return Some(Want::Props(keys(p)));
        }
        if root.child(DAV, "propname").is_some() {
            return Some(Want::Names);
        }
        if root.child(DAV, "allprop").is_some() {
            return Some(Want::All(
                root.child(DAV, "include").map(keys).unwrap_or_default(),
            ));
        }
        None
    }

    /// Запрошены данные объектов (`calendar-data`, `address-data`).
    pub(crate) fn needs_body(&self) -> bool {
        match self {
            Want::All(n) | Want::Props(n) => n.iter().any(is_data),
            Want::Names => false,
        }
    }
}

/// Имена дочерних элементов по порядку, без повторов.
pub(crate) fn keys(el: &Element) -> Vec<PropName> {
    let mut seen = BTreeSet::new();
    el.children
        .iter()
        .map(Element::key)
        .filter(|k| seen.insert(k.clone()))
        .collect()
}

fn is_data(n: &PropName) -> bool {
    n.is(CALDAV, "calendar-data") || n.is(CARDDAV, "address-data")
}

fn p(ns: &str, name: &str, value: String) -> (PropName, String) {
    (PropName::new(ns, name), value)
}

/// `Content-Type` объектов коллекции вида `kind`.
pub(crate) fn content_type(kind: CollectionKind) -> &'static str {
    match kind {
        CollectionKind::Calendar => "text/calendar; charset=utf-8",
        CollectionKind::AddressBook => "text/vcard; charset=utf-8",
    }
}

/// Все свойства ресурса со значениями (готовый XML).
fn properties(cx: &Cx<'_>, res: &Res<'_>) -> Vec<(PropName, String)> {
    let login = cx.login();
    let me = path::principal(login);
    let rt = match res {
        Res::Root | Res::Principals | Res::Home(_) => "<d:collection/>",
        Res::Principal => "<d:collection/><d:principal/>",
        Res::Collection(c) => match c.kind {
            CollectionKind::Calendar => "<d:collection/><c:calendar/>",
            CollectionKind::AddressBook => "<d:collection/><card:addressbook/>",
        },
        Res::Schedule(true) => "<d:collection/><c:schedule-inbox/>",
        Res::Schedule(false) => "<d:collection/><c:schedule-outbox/>",
        Res::Object { .. } => "",
    };
    let mut v = vec![p(DAV, "resourcetype", rt.to_owned())];
    match res {
        Res::Principal => {
            v.push(p(DAV, "displayname", text(login).into_owned()));
            v.push(p(DAV, "principal-URL", href(&me)));
            v.push(p(
                CALDAV,
                "calendar-home-set",
                href(&path::home(CollectionKind::Calendar, login)),
            ));
            v.push(p(
                CARDDAV,
                "addressbook-home-set",
                href(&path::home(CollectionKind::AddressBook, login)),
            ));
            let mut addrs = String::new();
            if login.contains('@') {
                addrs.push_str(&href(&format!("mailto:{login}")));
            }
            addrs.push_str(&href(&me));
            v.push(p(CALDAV, "calendar-user-address-set", addrs));
            v.push(p(
                CALDAV,
                "schedule-inbox-URL",
                href(&schedule_href(login, true)),
            ));
            v.push(p(
                CALDAV,
                "schedule-outbox-URL",
                href(&schedule_href(login, false)),
            ));
        }
        Res::Home(_) | Res::Schedule(_) => v.push(p(DAV, "owner", href(&me))),
        Res::Collection(c) => collection_props(cx, c, &me, &mut v),
        Res::Object {
            coll, etag, body, ..
        } => {
            v.push(p(DAV, "getetag", text(&format!("\"{etag}\"")).into_owned()));
            v.push(p(DAV, "getcontenttype", content_type(coll.kind).to_owned()));
            if let Some(b) = body {
                let name = match coll.kind {
                    CollectionKind::Calendar => PropName::new(CALDAV, "calendar-data"),
                    CollectionKind::AddressBook => PropName::new(CARDDAV, "address-data"),
                };
                v.push((name, text(b).into_owned()));
                v.push(p(DAV, "getcontentlength", b.len().to_string()));
            }
        }
        Res::Root | Res::Principals => {}
    }
    v.push(p(DAV, "current-user-principal", href(&me)));
    v.push(p(DAV, "principal-collection-set", href(path::PRINCIPALS)));
    let privs = match res {
        Res::Root | Res::Principals | Res::Principal => PRIV_READ,
        Res::Collection(c) | Res::Object { coll: c, .. } if gal::is(c) => PRIV_READ,
        _ => PRIV_OWNER,
    };
    v.push(p(DAV, "current-user-privilege-set", privs.to_owned()));
    let reports = match res {
        Res::Collection(c) if c.kind == CollectionKind::Calendar => concat!(
            "<d:supported-report><d:report><d:sync-collection/></d:report></d:supported-report>",
            "<d:supported-report><d:report><c:calendar-query/></d:report></d:supported-report>",
            "<d:supported-report><d:report><c:calendar-multiget/></d:report></d:supported-report>",
            "<d:supported-report><d:report><c:free-busy-query/></d:report></d:supported-report>"
        ),
        Res::Collection(_) => concat!(
            "<d:supported-report><d:report><d:sync-collection/></d:report></d:supported-report>",
            "<d:supported-report><d:report><card:addressbook-query/></d:report></d:supported-report>",
            "<d:supported-report><d:report><card:addressbook-multiget/></d:report></d:supported-report>"
        ),
        _ => "",
    };
    v.push(p(DAV, "supported-report-set", reports.to_owned()));
    v
}

fn collection_props(cx: &Cx<'_>, c: &Collection, me: &str, v: &mut Vec<(PropName, String)>) {
    v.push(p(DAV, "displayname", text(&c.display_name).into_owned()));
    v.push(p(DAV, "owner", href(me)));
    v.push(p(CS, "getctag", c.state.0.to_string()));
    v.push(p(DAV, "sync-token", sync_token(c.state)));
    let max = cx.dav.max_object_size().to_string();
    match c.kind {
        CollectionKind::Calendar => {
            if let Some(d) = &c.description {
                v.push(p(CALDAV, "calendar-description", text(d).into_owned()));
            }
            if let Some(color) = &c.color {
                v.push(p(ICAL, "calendar-color", text(color).into_owned()));
            }
            v.push(p(ICAL, "calendar-order", c.order.to_string()));
            let comps: String = c
                .components
                .iter()
                .map(|x| format!("<c:comp name=\"{x}\"/>"))
                .collect();
            v.push(p(CALDAV, "supported-calendar-component-set", comps));
            v.push(p(
                CALDAV,
                "supported-calendar-data",
                "<c:calendar-data content-type=\"text/calendar\" version=\"2.0\"/>".to_owned(),
            ));
            v.push(p(CALDAV, "max-resource-size", max));
            let (min, max) = cx.dav.cfg.date_bounds();
            for (name, bound) in [("min-date-time", min), ("max-date-time", max)] {
                if let Some(t) = bound {
                    let value = format!(
                        "{:04}{:02}{:02}T000000Z",
                        t.year(),
                        u8::from(t.month()),
                        t.day()
                    );
                    v.push(p(CALDAV, name, value));
                }
            }
            if let Some(tz) = &c.timezone {
                v.push(p(CALDAV, "calendar-timezone-id", text(tz).into_owned()));
                if let Some(ical) = vtimezone(tz) {
                    v.push(p(CALDAV, "calendar-timezone", text(&ical).into_owned()));
                }
            }
        }
        CollectionKind::AddressBook => {
            if let Some(d) = &c.description {
                v.push(p(CARDDAV, "addressbook-description", text(d).into_owned()));
            }
            v.push(p(
                CARDDAV,
                "supported-address-data",
                concat!(
                    "<card:address-data-type content-type=\"text/vcard\" version=\"3.0\"/>",
                    "<card:address-data-type content-type=\"text/vcard\" version=\"4.0\"/>"
                )
                .to_owned(),
            ));
            v.push(p(CARDDAV, "max-resource-size", max));
            v.push(p(
                CARDDAV,
                "max-image-size",
                cx.dav.cfg.max_photo_size.to_string(),
            ));
        }
    }
}

/// Идентификатор изготовителя в создаваемых данных iCalendar.
pub(crate) const PRODID: &str = "-//The Post//DAV//RU";

/// Пустой VCALENDAR (`VERSION`, `PRODID`).
pub(crate) fn vcalendar() -> ICalendar {
    let mut root = ICalendarComponent::new(ICalendarComponentType::VCalendar);
    root.add_property(ICalendarProperty::Version, "2.0");
    root.add_property(ICalendarProperty::Prodid, PRODID);
    ICalendar {
        components: vec![root],
    }
}

/// Значение `calendar-timezone` (RFC 4791 §5.2.2): VCALENDAR с VTIMEZONE пояса `tzid` (IANA), правила — на год
/// вперёд от текущего момента. Неизвестный пояс — `None`.
fn vtimezone(tzid: &str) -> Option<String> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let mut ical = vcalendar();
    ical.add_timezone(tzid, now, now + 366 * 86_400)?;
    Some(ical.to_string())
}

/// Часовой пояс (IANA) из значения `calendar-timezone` (VCALENDAR с VTIMEZONE).
fn timezone_of(ical: &str) -> Option<String> {
    let ical = ICalendar::parse(ical).ok()?;
    let (_, tz) = ical.timezones().find_map(ICalendarComponent::timezone)?;
    tz.name().map(std::borrow::Cow::into_owned)
}

/// Группы `propstat` ресурса для запроса `want`.
fn groups(cx: &Cx<'_>, res: &Res<'_>, want: &Want) -> Vec<Group> {
    let all = properties(cx, res);
    let find = |n: &PropName| all.iter().find(|(k, _)| k == n).cloned();
    let mut ok: Vec<(PropName, String)> = Vec::new();
    let mut missing: Vec<(PropName, String)> = Vec::new();
    match want {
        Want::Names => {
            ok = all
                .iter()
                .map(|(n, _)| (n.clone(), String::new()))
                .collect();
        }
        Want::All(extra) => {
            ok = all
                .iter()
                .filter(|(n, _)| ALLPROP.iter().any(|(ns, name)| n.is(ns, name)))
                .cloned()
                .collect();
            for n in extra {
                if ok.iter().any(|(k, _)| k == n) {
                    continue;
                }
                match find(n) {
                    Some(x) => ok.push(x),
                    None => missing.push((n.clone(), String::new())),
                }
            }
        }
        Want::Props(names) => {
            for n in names {
                match find(n) {
                    Some(x) => ok.push(x),
                    None => missing.push((n.clone(), String::new())),
                }
            }
        }
    }
    vec![(StatusCode::OK, ok), (StatusCode::NOT_FOUND, missing)]
}

/// `response` ресурса.
pub(crate) fn emit(cx: &Cx<'_>, ms: &mut Multistatus, res: &Res<'_>, want: &Want) {
    ms.props(&res.href(cx.login()), &groups(cx, res, want));
}

/// Ответ урезан лимитом: `507` с `number-of-matches-within-limits` для ресурса запроса (RFC 6578 §3.6).
pub(crate) fn truncated(ms: &mut Multistatus, href: &str) {
    ms.status(
        href,
        StatusCode::INSUFFICIENT_STORAGE,
        Some("<d:number-of-matches-within-limits/>"),
    );
}

/// `PROPFIND`.
pub(crate) async fn propfind(cx: &Cx<'_>, target: &Target, body: Body) -> Result<Response> {
    let depth1 = match cx.header("depth") {
        Some("0") => false,
        Some("1") => true,
        Some(d) if !d.eq_ignore_ascii_case("infinity") => {
            return Ok(status(StatusCode::BAD_REQUEST));
        }
        _ => {
            return Ok(precondition(
                StatusCode::FORBIDDEN,
                "<d:propfind-finite-depth/>",
            ));
        }
    };
    let el = match cx.xml(body).await {
        Ok(el) => el,
        Err(code) => return Ok(status(code)),
    };
    let Some(want) = Want::parse(el.as_ref()) else {
        return Ok(status(StatusCode::BAD_REQUEST));
    };
    let mut ms = Multistatus::new();
    let max = cx.dav.cfg.max_response_items;
    match target {
        Target::Root => {
            emit(cx, &mut ms, &Res::Root, &want);
            if depth1 {
                emit(cx, &mut ms, &Res::Principals, &want);
            }
        }
        Target::Principals => {
            emit(cx, &mut ms, &Res::Principals, &want);
            if depth1 {
                emit(cx, &mut ms, &Res::Principal, &want);
            }
        }
        Target::Principal(_) => emit(cx, &mut ms, &Res::Principal, &want),
        Target::Home(kind, _) => {
            ensure_defaults(cx, *kind).await?;
            emit(cx, &mut ms, &Res::Home(*kind), &want);
            if depth1 {
                let list = cx.store().list_collections(cx.scope, *kind).await?;
                for c in list.iter().take(max) {
                    emit(cx, &mut ms, &Res::Collection(c), &want);
                }
                if list.len() > max {
                    truncated(&mut ms, &target.href(cx.login()));
                }
                // Каталог недоступен — дом показывается без глобальной адресной книги.
                if *kind == CollectionKind::AddressBook {
                    match gal::load(cx).await {
                        Ok(Some(g)) => emit(cx, &mut ms, &Res::Collection(&g.coll), &want),
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "DAV: глобальная адресная книга недоступна");
                        }
                    }
                }
            }
        }
        Target::Collection(kind, _, name) => {
            if let Some(g) = gal::find(cx, *kind, name).await? {
                emit(cx, &mut ms, &Res::Collection(&g.coll), &want);
                if depth1 {
                    for o in &g.cards {
                        gal::emit(cx, &mut ms, &g, o, &want);
                    }
                }
                return Ok(multistatus(ms));
            }
            let Some(c) = cx.store().collection_by_name(cx.scope, *kind, name).await? else {
                if *kind == CollectionKind::Calendar && path::reserved(*kind, name) {
                    emit(cx, &mut ms, &Res::Schedule(name == path::INBOX), &want);
                    return Ok(multistatus(ms));
                }
                return Ok(status(StatusCode::NOT_FOUND));
            };
            emit(cx, &mut ms, &Res::Collection(&c), &want);
            if depth1 {
                children(cx, &mut ms, &c, &want).await?;
            }
        }
        Target::Object(kind, _, coll, name) => {
            if let Some(g) = gal::find(cx, *kind, coll).await? {
                let Some(o) = g.card(name) else {
                    return Ok(status(StatusCode::NOT_FOUND));
                };
                gal::emit(cx, &mut ms, &g, o, &want);
                return Ok(multistatus(ms));
            }
            let Some(c) = cx.store().collection_by_name(cx.scope, *kind, coll).await? else {
                return Ok(status(StatusCode::NOT_FOUND));
            };
            let Some(o) = cx.store().get_object(cx.scope, c.id, name).await? else {
                return Ok(status(StatusCode::NOT_FOUND));
            };
            let res = Res::Object {
                coll: &c,
                name: &o.name,
                etag: &o.etag,
                body: Some(&o.body),
            };
            emit(cx, &mut ms, &res, &want);
        }
    }
    Ok(multistatus(ms))
}

/// Коллекции по умолчанию дома вида `kind` — если у учётной записи нет ни одной коллекции этого вида: календарь
/// и список задач либо адресная книга. Имена ресурсов постоянные ([`path::DEFAULT_CALENDAR`],
/// [`path::DEFAULT_TASKS`], [`path::DEFAULT_ADDRESSBOOK`]): одновременные обращения не создают дубликатов
/// (`Conflict` хранилища — уже создано).
pub(crate) async fn ensure_defaults(cx: &Cx<'_>, kind: CollectionKind) -> Result<()> {
    defaults(cx.store(), cx.scope, &cx.dav.cfg, kind).await
}

/// [`ensure_defaults`] без запроса HTTP (приём iTIP).
pub(crate) async fn defaults(
    store: &dyn DavStore,
    scope: AccountScope,
    cfg: &DavConfig,
    kind: CollectionKind,
) -> Result<()> {
    if !store.list_collections(scope, kind).await?.is_empty() {
        return Ok(());
    }
    let defaults: &[(&str, &String, Component)] = match kind {
        CollectionKind::Calendar => &[
            (
                path::DEFAULT_CALENDAR,
                &cfg.default_calendar_name,
                Component::Event,
            ),
            (
                path::DEFAULT_TASKS,
                &cfg.default_tasks_name,
                Component::Todo,
            ),
        ],
        CollectionKind::AddressBook => &[(
            path::DEFAULT_ADDRESSBOOK,
            &cfg.default_addressbook_name,
            Component::Card,
        )],
    };
    for (order, (name, display, component)) in (0u32..).zip(defaults) {
        let c = Collection {
            display_name: (*display).clone(),
            order,
            components: BTreeSet::from([*component]),
            ..Collection::new(kind, *name)
        };
        match store.create_collection(scope, c).await {
            Ok(_) | Err(Error::Conflict) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Объекты коллекции (`Depth: 1`), не больше `dav.max_response_items`.
async fn children(cx: &Cx<'_>, ms: &mut Multistatus, c: &Collection, want: &Want) -> Result<()> {
    let max = cx.dav.cfg.max_response_items;
    let refs = cx.store().list_objects(cx.scope, c.id).await?;
    let page = &refs[..refs.len().min(max)];
    if want.needs_body() {
        let names: Vec<String> = page.iter().map(|r| r.name.clone()).collect();
        for o in cx.store().multiget(cx.scope, c.id, &names).await? {
            let res = Res::Object {
                coll: c,
                name: &o.name,
                etag: &o.etag,
                body: Some(&o.body),
            };
            emit(cx, ms, &res, want);
        }
    } else {
        for r in page {
            let res = Res::Object {
                coll: c,
                name: &r.name,
                etag: &r.etag,
                body: None,
            };
            emit(cx, ms, &res, want);
        }
    }
    if refs.len() > max {
        truncated(ms, &path::collection(c.kind, cx.login(), &c.name));
    }
    Ok(())
}

/// Операции `propertyupdate` по порядку: свойство и новое значение (`None` — `remove`). Неверное тело — `None`.
pub(crate) fn updates(root: &Element) -> Option<Vec<(PropName, Option<&Element>)>> {
    if !root.is(DAV, "propertyupdate") {
        return None;
    }
    let mut ops = Vec::new();
    for op in &root.children {
        let set = if op.is(DAV, "set") {
            true
        } else if op.is(DAV, "remove") {
            false
        } else {
            continue;
        };
        for prop in op.children.iter().filter(|c| c.is(DAV, "prop")) {
            for e in &prop.children {
                ops.push((e.key(), set.then_some(e)));
            }
        }
    }
    Some(ops)
}

/// Применяет к коллекции установку (`Some`) или удаление (`None`) свойства. Статус: 200 — применено, 403 —
/// защищённое или неизвестное, 409 — недопустимое значение (коллекция не изменена).
pub(crate) fn apply(c: &mut Collection, name: &PropName, value: Option<&Element>) -> StatusCode {
    let prev = c.clone();
    let v = value.map(|e| e.trimmed().to_owned());
    let calendar = c.kind == CollectionKind::Calendar;
    match (name.ns.as_str(), name.name.as_str()) {
        (DAV, "displayname") => {
            let default = c.name.clone();
            c.display_name = v.unwrap_or(default);
        }
        (CALDAV, "calendar-description") if calendar => {
            c.description = v.filter(|s| !s.is_empty());
        }
        (CARDDAV, "addressbook-description") if !calendar => {
            c.description = v.filter(|s| !s.is_empty());
        }
        (ICAL, "calendar-color") if calendar => c.color = v.filter(|s| !s.is_empty()),
        (ICAL, "calendar-order") if calendar => match v.as_deref().map(str::parse::<u32>) {
            None => c.order = 0,
            Some(Ok(n)) => c.order = n,
            Some(Err(_)) => return StatusCode::CONFLICT,
        },
        (CALDAV, "calendar-timezone") if calendar => match v.filter(|s| !s.is_empty()) {
            None => c.timezone = None,
            // Пояс, которого нет в базе IANA (своё определение клиента), не хранится: календарь остаётся в UTC.
            Some(ical) => c.timezone = timezone_of(&ical),
        },
        (CALDAV, "calendar-timezone-id") if calendar => match v.filter(|s| !s.is_empty()) {
            None => c.timezone = None,
            Some(id) => match Tz::from_str(&id).ok().and_then(|tz| tz.name()) {
                Some(tz) => c.timezone = Some(tz.into_owned()),
                None => return StatusCode::CONFLICT,
            },
        },
        _ => return StatusCode::FORBIDDEN,
    }
    if c.validate().is_err() {
        *c = prev;
        return StatusCode::CONFLICT;
    }
    StatusCode::OK
}

/// Свойства со статусами — группы `propstat` по статусам в порядке первого появления.
pub(crate) fn status_groups(results: Vec<(PropName, StatusCode)>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for (n, s) in results {
        match groups.iter_mut().find(|(st, _)| *st == s) {
            Some(g) => g.1.push((n, String::new())),
            None => groups.push((s, vec![(n, String::new())])),
        }
    }
    groups
}

/// `PROPPATCH`.
pub(crate) async fn proppatch(cx: &Cx<'_>, target: &Target, body: Body) -> Result<Response> {
    let el = match cx.xml(body).await {
        Ok(el) => el,
        Err(code) => return Ok(status(code)),
    };
    let Some(ops) = el.as_ref().and_then(updates) else {
        return Ok(status(StatusCode::BAD_REQUEST));
    };
    let results: Vec<(PropName, StatusCode)> = match target {
        Target::Collection(kind, _, name) => {
            let Some(mut c) = cx.store().collection_by_name(cx.scope, *kind, name).await? else {
                return Ok(status(StatusCode::NOT_FOUND));
            };
            let mut res: Vec<_> = ops
                .iter()
                .map(|(n, v)| (n.clone(), apply(&mut c, n, *v)))
                .collect();
            if res.iter().all(|(_, s)| *s == StatusCode::OK) {
                if !res.is_empty() {
                    cx.store().update_collection(cx.scope, c).await?;
                }
            } else {
                for (_, s) in &mut res {
                    if *s == StatusCode::OK {
                        *s = StatusCode::FAILED_DEPENDENCY;
                    }
                }
            }
            res
        }
        _ => ops
            .into_iter()
            .map(|(n, _)| (n, StatusCode::FORBIDDEN))
            .collect(),
    };
    let mut ms = Multistatus::new();
    ms.props(&target.href(cx.login()), &status_groups(results));
    Ok(multistatus(ms))
}
