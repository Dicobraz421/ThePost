//! Пространство URL DAV под `/dav/`: разбор пути запроса и ссылки `href` ответов.
//!
//! ```text
//! /dav/                                         корень (current-user-principal)
//! /dav/principals/                              коллекция принципалов
//! /dav/principals/<логин>/                      принципал
//! /dav/principals/<логин>/calendar-proxy-read/  представители владельца: чтение (CalendarServer)
//! /dav/principals/<логин>/calendar-proxy-write/ представители владельца: чтение и запись
//! /dav/calendars/<логин>/                       дом календарей
//! /dav/calendars/<логин>/<коллекция>/           календарь
//! /dav/calendars/<логин>/<коллекция>/<объект>   событие, задача
//! /dav/addressbooks/<логин>/…                   то же для адресных книг
//! /dav/addressbooks/<логин>/gal/                глобальная адресная книга (только чтение)
//! ```
//!
//! Сегменты декодируются из процентной записи (UTF-8); пустые сегменты пропускаются, коллекции — со `/` и без.

use std::fmt::Write as _;

use tp_contracts::dav::{CollectionKind, GAL_COLLECTION_NAME};

/// Префикс пространства DAV.
pub const PREFIX: &str = "/dav/";

/// Ресурс запроса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// `/dav/`.
    Root,
    /// `/dav/principals/`.
    Principals,
    /// Принципал: логин из URL.
    Principal(String),
    /// Группа представителей принципала (`calendar-proxy-read`, `calendar-proxy-write`): логин владельца, запись.
    Proxy(String, bool),
    /// Дом календарей или адресных книг: вид, логин.
    Home(CollectionKind, String),
    /// Коллекция: вид, логин, имя.
    Collection(CollectionKind, String, String),
    /// Объект: вид, логин, коллекция, имя ресурса.
    Object(CollectionKind, String, String, String),
}

impl Target {
    /// Ресурс по пути; `None` — вне пространства DAV, неизвестный или неверно закодированный путь (404).
    pub(crate) fn parse(path: &str) -> Option<Target> {
        let rest = path.strip_prefix("/dav")?;
        if !(rest.is_empty() || rest.starts_with('/')) {
            return None;
        }
        let mut segs = Vec::new();
        for s in rest.split('/').filter(|s| !s.is_empty()) {
            segs.push(decode(s)?);
        }
        let mut it = segs.into_iter();
        let Some(area) = it.next() else {
            return Some(Target::Root);
        };
        let kind = match area.as_str() {
            "principals" => None,
            "calendars" => Some(CollectionKind::Calendar),
            "addressbooks" => Some(CollectionKind::AddressBook),
            _ => return None,
        };
        let (login, coll, obj) = (it.next(), it.next(), it.next());
        if it.next().is_some() {
            return None;
        }
        Some(match (kind, login, coll, obj) {
            (None, None, ..) => Target::Principals,
            (None, Some(l), None, _) => Target::Principal(l),
            (None, Some(l), Some(g), None) if g == PROXY_READ || g == PROXY_WRITE => {
                Target::Proxy(l, g == PROXY_WRITE)
            }
            (None, Some(_), Some(_), _) | (Some(_), None, ..) => return None,
            (Some(k), Some(l), None, _) => Target::Home(k, l),
            (Some(k), Some(l), Some(c), None) => Target::Collection(k, l, c),
            (Some(k), Some(l), Some(c), Some(o)) => Target::Object(k, l, c, o),
        })
    }

    /// Ссылка на ресурс с логином вошедшего `login`.
    pub(crate) fn href(&self, login: &str) -> String {
        match self {
            Target::Root => PREFIX.to_owned(),
            Target::Principals => PRINCIPALS.to_owned(),
            Target::Principal(_) => principal(login),
            Target::Proxy(_, write) => proxy(login, *write),
            Target::Home(k, _) => home(*k, login),
            Target::Collection(k, _, c) => collection(*k, login, c),
            Target::Object(k, _, c, o) => object(*k, login, c, o),
        }
    }

    /// Логин владельца из URL.
    pub(crate) fn login(&self) -> Option<&str> {
        match self {
            Target::Root | Target::Principals => None,
            Target::Principal(l)
            | Target::Proxy(l, _)
            | Target::Home(_, l)
            | Target::Collection(_, l, _)
            | Target::Object(_, l, _, _) => Some(l),
        }
    }
}

/// Процентное декодирование сегмента; неверная запись или не UTF-8 — `None`.
fn decode(seg: &str) -> Option<String> {
    let b = seg.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = seg.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Процентная запись сегмента: без изменений — буквы и цифры ASCII, `-._~@`.
pub(crate) fn encode(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~@".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn area(kind: CollectionKind) -> &'static str {
    match kind {
        CollectionKind::Calendar => "calendars",
        CollectionKind::AddressBook => "addressbooks",
    }
}

/// Имя ресурса входящих приглашений в доме календарей (`schedule-inbox-URL`, RFC 6638; заготовка для DAV-13).
pub(crate) const INBOX: &str = "inbox";

/// Имя ресурса исходящих приглашений в доме календарей (`schedule-outbox-URL`).
pub(crate) const OUTBOX: &str = "outbox";

/// Имя зарезервировано: ресурсы планирования дома календарей, глобальная адресная книга дома адресных книг
/// ([`GAL_COLLECTION_NAME`]).
pub(crate) fn reserved(kind: CollectionKind, name: &str) -> bool {
    match kind {
        CollectionKind::Calendar => name == INBOX || name == OUTBOX,
        CollectionKind::AddressBook => name == GAL_COLLECTION_NAME,
    }
}

/// Имя календаря по умолчанию (сегмент URL).
pub(crate) const DEFAULT_CALENDAR: &str = "calendar";

/// Имя списка задач по умолчанию (сегмент URL).
pub(crate) const DEFAULT_TASKS: &str = "tasks";

/// Имя адресной книги по умолчанию (сегмент URL).
pub(crate) const DEFAULT_ADDRESSBOOK: &str = "contacts";

/// `/dav/principals/`.
pub(crate) const PRINCIPALS: &str = "/dav/principals/";

/// `/dav/principals/<логин>/`.
pub(crate) fn principal(login: &str) -> String {
    format!("{PRINCIPALS}{}/", encode(login))
}

/// Сегмент группы представителей с правом чтения (CalendarServer `calendar-proxy-read`).
pub(crate) const PROXY_READ: &str = "calendar-proxy-read";

/// Сегмент группы представителей с правом записи (`calendar-proxy-write`).
pub(crate) const PROXY_WRITE: &str = "calendar-proxy-write";

/// `/dav/principals/<логин>/calendar-proxy-{read,write}/`.
pub(crate) fn proxy(login: &str, write: bool) -> String {
    let group = if write { PROXY_WRITE } else { PROXY_READ };
    format!("{}{group}/", principal(login))
}

/// Логин принципала из ссылки `href` (путь или абсолютный URL `…/dav/principals/<логин>/`); иначе `None`.
pub(crate) fn principal_login(href: &str) -> Option<String> {
    let path = match href.find("://") {
        Some(i) => &href[i + 3..][href[i + 3..].find('/')?..],
        None => href,
    };
    match Target::parse(path)? {
        Target::Principal(l) => Some(l),
        _ => None,
    }
}

/// `/dav/<вид>/<логин>/`.
pub(crate) fn home(kind: CollectionKind, login: &str) -> String {
    format!("{PREFIX}{}/{}/", area(kind), encode(login))
}

/// `/dav/<вид>/<логин>/<коллекция>/`.
pub(crate) fn collection(kind: CollectionKind, login: &str, name: &str) -> String {
    format!("{}{}/", home(kind, login), encode(name))
}

/// `/dav/<вид>/<логин>/<коллекция>/<объект>`.
pub(crate) fn object(kind: CollectionKind, login: &str, coll: &str, name: &str) -> String {
    format!("{}{}", collection(kind, login, coll), encode(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use CollectionKind::{AddressBook, Calendar};

    #[test]
    fn parse() {
        let s = |v: &str| v.to_owned();
        let cases = [
            ("/dav", Some(Target::Root)),
            ("/dav/", Some(Target::Root)),
            ("/dav/principals", Some(Target::Principals)),
            (
                "/dav/principals/a@b.ru/",
                Some(Target::Principal(s("a@b.ru"))),
            ),
            ("/dav/calendars/a/", Some(Target::Home(Calendar, s("a")))),
            (
                "/dav/addressbooks/a/book",
                Some(Target::Collection(AddressBook, s("a"), s("book"))),
            ),
            (
                "/dav/calendars/a/%D0%A0%20x/e%2Fv.ics",
                Some(Target::Object(Calendar, s("a"), s("Р x"), s("e/v.ics"))),
            ),
            ("/davx/", None),
            ("/dav/other/", None),
            ("/dav/calendars/", None),
            ("/dav/principals/a/b", None),
            (
                "/dav/principals/a/calendar-proxy-write/",
                Some(Target::Proxy(s("a"), true)),
            ),
            (
                "/dav/principals/a/calendar-proxy-read",
                Some(Target::Proxy(s("a"), false)),
            ),
            ("/dav/principals/a/calendar-proxy-read/x", None),
            ("/dav/calendars/a/b/c/d", None),
            ("/dav/calendars/a/%zz/", None),
            ("/dav/calendars/a/%ff/", None),
        ];
        for (path, want) in cases {
            assert_eq!(Target::parse(path), want, "{path}");
        }
    }

    #[test]
    fn hrefs() {
        assert_eq!(
            principal("ivan@example.org"),
            "/dav/principals/ivan@example.org/"
        );
        assert_eq!(
            object(Calendar, "a", "Мой календарь", "x y.ics"),
            "/dav/calendars/a/%D0%9C%D0%BE%D0%B9%20%D0%BA%D0%B0%D0%BB%D0%B5%D0%BD%D0%B4%D0%B0%D1%80%D1%8C/x%20y.ics"
        );
        assert_eq!(home(AddressBook, "a"), "/dav/addressbooks/a/");
        assert_eq!(
            proxy("a@b.ru", true),
            "/dav/principals/a@b.ru/calendar-proxy-write/"
        );
        assert_eq!(
            principal_login("https://mail.example.org/dav/principals/a%40b.ru/").as_deref(),
            Some("a@b.ru")
        );
        assert_eq!(principal_login("/dav/principals/a/").as_deref(), Some("a"));
        assert_eq!(principal_login("/dav/calendars/a/"), None);
        assert_eq!(principal_login("mailto:a@b.ru"), None);
        let back = Target::parse(&object(Calendar, "a", "Мой/календарь", "x?y.ics"));
        assert_eq!(
            back,
            Some(Target::Object(
                Calendar,
                "a".into(),
                "Мой/календарь".into(),
                "x?y.ics".into()
            ))
        );
    }
}
