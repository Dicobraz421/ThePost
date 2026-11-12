//! XML WebDAV: разбор тел запросов (quick-xml) в дерево с лимитами и запись ответов `multistatus` (RFC 4918 §13–14).
//!
//! Разбор: только UTF-8; объявление DTD — ошибка (внешние сущности и «billion laughs» невозможны), из ссылок на
//! сущности — только предопределённые XML и символьные; глубина и число элементов — [`XmlLimits`].
//! Пространства имён разрешаются ([`Element::ns`]); имена атрибутов — без префикса.
//!
//! Запись: корневой элемент объявляет префиксы `d` (`DAV:`), `c` (CalDAV), `card` (CardDAV), `cs` (CalendarServer),
//! `ical` (Apple); свойство другого пространства объявляет его на себе (`x`).

use std::borrow::Cow;
use std::fmt::Write as _;

use axum::http::StatusCode;
use quick_xml::NsReader;
use quick_xml::XmlVersion;
use quick_xml::escape::{escape, partial_escape, resolve_predefined_entity};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;

/// `DAV:` (RFC 4918).
pub const DAV: &str = "DAV:";
/// CalDAV (RFC 4791).
pub const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
/// CardDAV (RFC 6352).
pub const CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
/// CalendarServer (`getctag`).
pub const CS: &str = "http://calendarserver.org/ns/";
/// Apple iCal (`calendar-color`, `calendar-order`).
pub const ICAL: &str = "http://apple.com/ns/ical/";

/// Префиксы известных пространств в ответах.
const PREFIXES: [(&str, &str); 5] = [
    (DAV, "d"),
    (CALDAV, "c"),
    (CARDDAV, "card"),
    (CS, "cs"),
    (ICAL, "ical"),
];

/// Объявления префиксов корневого элемента ответа.
pub(crate) const XMLNS: &str = concat!(
    r#" xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav""#,
    r#" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/""#,
    r#" xmlns:ical="http://apple.com/ns/ical/""#
);

/// Заголовок документа ответа.
pub(crate) const HEADER: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n";

/// Лимиты разбора.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XmlLimits {
    /// Глубина вложенности (корень — 1).
    pub max_depth: usize,
    /// Элементов в документе.
    pub max_elements: usize,
}

/// Ошибка разбора тела.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum XmlError {
    /// Тело не в UTF-8.
    #[error("тело не в UTF-8")]
    Utf8,
    /// Объявление DTD.
    #[error("DTD не поддерживается")]
    Dtd,
    /// Глубже лимита.
    #[error("глубина XML больше {0}")]
    Depth(usize),
    /// Элементов больше лимита.
    #[error("элементов XML больше {0}")]
    Elements(usize),
    /// Неизвестная сущность или необъявленный префикс.
    #[error("неизвестная ссылка: {0}")]
    Reference(String),
    /// Нет корневого элемента.
    #[error("нет корневого элемента")]
    NoRoot,
    /// Нарушен синтаксис.
    #[error("разбор XML: {0}")]
    Syntax(String),
}

/// Элемент разобранного тела.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Element {
    /// Пространство имён; пустое — без пространства.
    pub ns: String,
    /// Локальное имя.
    pub name: String,
    /// Атрибуты: локальное имя и значение (объявления `xmlns` пропущены).
    pub attrs: Vec<(String, String)>,
    /// Дочерние элементы по порядку.
    pub children: Vec<Element>,
    /// Текст элемента (все текстовые узлы подряд, без обрезки).
    pub text: String,
}

impl Element {
    /// Элемент `{ns}name`.
    pub fn is(&self, ns: &str, name: &str) -> bool {
        self.ns == ns && self.name == name
    }

    /// Первый дочерний `{ns}name`.
    pub fn child(&self, ns: &str, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.is(ns, name))
    }

    /// Значение атрибута.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Текст без пробелов по краям.
    pub fn trimmed(&self) -> &str {
        self.text.trim()
    }

    /// Имя свойства элемента.
    pub fn key(&self) -> PropName {
        PropName::new(&self.ns, &self.name)
    }

    /// Глубина поддерева (сам элемент — 1).
    pub fn depth(&self) -> usize {
        1 + self.children.iter().map(Element::depth).max().unwrap_or(0)
    }

    /// Элементов в поддереве, включая этот.
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(Element::count).sum::<usize>()
    }
}

/// Разбирает тело запроса в дерево.
pub fn parse(body: &[u8], limits: XmlLimits) -> Result<Element, XmlError> {
    let text = std::str::from_utf8(body).map_err(|_| XmlError::Utf8)?;
    let mut reader = NsReader::from_str(text);
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    let mut count = 0usize;
    loop {
        let (res, event) = reader
            .read_resolved_event()
            .map_err(|e| XmlError::Syntax(e.to_string()))?;
        let ns = match res {
            ResolveResult::Bound(ns) => Ok(ns.into_inner().to_owned()),
            ResolveResult::Unbound => Ok(String::new()),
            ResolveResult::Unknown(prefix) => Err(prefix),
        };
        match event {
            Event::Start(e) => {
                let el = element(ns, &e, &mut count, stack.len(), limits)?;
                stack.push(el);
            }
            Event::Empty(e) => {
                let el = element(ns, &e, &mut count, stack.len(), limits)?;
                attach(&mut stack, &mut root, el)?;
            }
            Event::End(_) => {
                let el = stack
                    .pop()
                    .ok_or_else(|| XmlError::Syntax("лишний закрывающий тег".into()))?;
                attach(&mut stack, &mut root, el)?;
            }
            Event::Text(t) => push_text(&mut stack, &t.xml10_content()),
            Event::CData(t) => push_text(&mut stack, &t.xml10_content()),
            Event::GeneralRef(r) => {
                let resolved: Cow<'_, str> = match r
                    .resolve_char_ref()
                    .map_err(|e| XmlError::Reference(e.to_string()))?
                {
                    Some(c) => Cow::Owned(c.to_string()),
                    None => {
                        let name = r.xml10_content();
                        Cow::Borrowed(
                            resolve_predefined_entity(&name)
                                .ok_or_else(|| XmlError::Reference(name.into_owned()))?,
                        )
                    }
                };
                push_text(&mut stack, &resolved);
            }
            Event::DocType(_) => return Err(XmlError::Dtd),
            Event::Comment(_) | Event::Decl(_) | Event::PI(_) => {}
            Event::Eof => break,
        }
    }
    if !stack.is_empty() {
        return Err(XmlError::Syntax("незакрытый элемент".into()));
    }
    root.ok_or(XmlError::NoRoot)
}

fn element(
    ns: Result<String, String>,
    e: &BytesStart<'_>,
    count: &mut usize,
    open: usize,
    limits: XmlLimits,
) -> Result<Element, XmlError> {
    let ns = ns.map_err(XmlError::Reference)?;
    *count += 1;
    if *count > limits.max_elements {
        return Err(XmlError::Elements(limits.max_elements));
    }
    if open + 1 > limits.max_depth {
        return Err(XmlError::Depth(limits.max_depth));
    }
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|err| XmlError::Syntax(err.to_string()))?;
        let key = a.key.0;
        if key == "xmlns" || key.starts_with("xmlns:") {
            continue;
        }
        let value = a
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|err| XmlError::Syntax(err.to_string()))?
            .into_owned();
        attrs.push((a.key.local_name().as_ref().to_owned(), value));
    }
    Ok(Element {
        ns,
        name: e.local_name().as_ref().to_owned(),
        attrs,
        children: Vec::new(),
        text: String::new(),
    })
}

fn attach(stack: &mut [Element], root: &mut Option<Element>, el: Element) -> Result<(), XmlError> {
    match stack.last_mut() {
        Some(parent) => parent.children.push(el),
        None if root.is_some() => {
            return Err(XmlError::Syntax("второй корневой элемент".into()));
        }
        None => *root = Some(el),
    }
    Ok(())
}

fn push_text(stack: &mut [Element], text: &str) {
    if let Some(top) = stack.last_mut() {
        top.text.push_str(text);
    }
}

// ---------- Запись ----------

/// Имя свойства: пространство и локальное имя.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PropName {
    /// Пространство имён.
    pub ns: String,
    /// Локальное имя.
    pub name: String,
}

impl PropName {
    /// Имя `{ns}name`.
    pub fn new(ns: &str, name: &str) -> Self {
        Self {
            ns: ns.to_owned(),
            name: name.to_owned(),
        }
    }

    /// Имя `{ns}name` — то же.
    pub fn is(&self, ns: &str, name: &str) -> bool {
        self.ns == ns && self.name == name
    }

    fn prefix(&self) -> Option<&'static str> {
        PREFIXES
            .iter()
            .find(|(ns, _)| *ns == self.ns)
            .map(|(_, p)| *p)
    }

    fn open(&self, out: &mut String) {
        let _ = match self.prefix() {
            Some(p) => write!(out, "<{p}:{}", self.name),
            None if self.ns.is_empty() => write!(out, "<{}", self.name),
            None => write!(out, "<x:{} xmlns:x=\"{}\"", self.name, escape(&self.ns)),
        };
    }

    fn close(&self, out: &mut String) {
        let _ = match self.prefix() {
            Some(p) => write!(out, "</{p}:{}>", self.name),
            None if self.ns.is_empty() => write!(out, "</{}>", self.name),
            None => write!(out, "</x:{}>", self.name),
        };
    }

    /// Элемент свойства со значением `inner` (готовый XML); пустое значение — пустой элемент.
    pub(crate) fn write(&self, out: &mut String, inner: &str) {
        self.open(out);
        if inner.is_empty() {
            out.push_str("/>");
        } else {
            out.push('>');
            out.push_str(inner);
            self.close(out);
        }
    }
}

/// Текст элемента XML: `&`, `<`, `>` экранированы.
pub(crate) fn text(s: &str) -> Cow<'_, str> {
    partial_escape(s)
}

/// `<d:href>…</d:href>`.
pub(crate) fn href(h: &str) -> String {
    format!("<d:href>{}</d:href>", text(h))
}

/// Строка статуса `HTTP/1.1 200 OK`.
pub(crate) fn status_line(s: StatusCode) -> String {
    format!(
        "HTTP/1.1 {} {}",
        s.as_u16(),
        s.canonical_reason().unwrap_or("")
    )
}

/// Свойства со значениями (готовый XML) с одним статусом.
pub(crate) type Group = (StatusCode, Vec<(PropName, String)>);

/// Группы `propstat`; пустые группы пропускаются, все пустые — один `200` с пустым `prop`.
pub(crate) fn write_propstats(out: &mut String, groups: &[Group]) {
    let mut any = false;
    for (status, props) in groups.iter().filter(|(_, p)| !p.is_empty()) {
        any = true;
        out.push_str("<d:propstat><d:prop>");
        for (name, value) in props {
            name.write(out, value);
        }
        let _ = write!(
            out,
            "</d:prop><d:status>{}</d:status></d:propstat>",
            status_line(*status)
        );
    }
    if !any {
        let _ = write!(
            out,
            "<d:propstat><d:prop/><d:status>{}</d:status></d:propstat>",
            status_line(StatusCode::OK)
        );
    }
}

/// Тело `<d:error>` с элементами предусловия `inner` (RFC 4918 §16).
pub(crate) fn error_body(inner: &str) -> String {
    format!("{HEADER}<d:error{XMLNS}>{inner}</d:error>")
}

/// Ответ `multistatus` (207).
#[derive(Debug)]
pub(crate) struct Multistatus {
    out: String,
}

impl Multistatus {
    pub(crate) fn new() -> Self {
        Self {
            out: format!("{HEADER}<d:multistatus{XMLNS}>"),
        }
    }

    /// `response` со свойствами.
    pub(crate) fn props(&mut self, h: &str, groups: &[Group]) {
        self.out.push_str("<d:response>");
        self.out.push_str(&href(h));
        write_propstats(&mut self.out, groups);
        self.out.push_str("</d:response>");
    }

    /// `response` со статусом ресурса и, если есть, предусловием.
    pub(crate) fn status(&mut self, h: &str, status: StatusCode, error: Option<&str>) {
        let _ = write!(
            self.out,
            "<d:response>{}<d:status>{}</d:status>",
            href(h),
            status_line(status)
        );
        if let Some(e) = error {
            let _ = write!(self.out, "<d:error>{e}</d:error>");
        }
        self.out.push_str("</d:response>");
    }

    /// `sync-token` ответа `sync-collection`.
    pub(crate) fn sync_token(&mut self, token: &str) {
        let _ = write!(self.out, "<d:sync-token>{}</d:sync-token>", text(token));
    }

    pub(crate) fn finish(mut self) -> String {
        self.out.push_str("</d:multistatus>");
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: XmlLimits = XmlLimits {
        max_depth: 8,
        max_elements: 50,
    };

    #[test]
    fn namespaces_and_text() {
        let el = parse(
            br#"<?xml version="1.0"?>
<D:propfind xmlns:D="DAV:" xmlns="urn:ietf:params:xml:ns:caldav">
  <D:prop><calendar-home-set/><D:displayname>A &amp; B &#x41;<![CDATA[<c>]]></D:displayname>
  <x:y xmlns:x="urn:z" attr="1 &lt; 2"/><plain xmlns=""/></D:prop>
</D:propfind>"#,
            LIMITS,
        )
        .expect("parse");
        assert!(el.is(DAV, "propfind"));
        let prop = el.child(DAV, "prop").expect("prop");
        let keys: Vec<_> = prop.children.iter().map(Element::key).collect();
        assert_eq!(
            keys,
            [
                PropName::new(CALDAV, "calendar-home-set"),
                PropName::new(DAV, "displayname"),
                PropName::new("urn:z", "y"),
                PropName::new("", "plain"),
            ]
        );
        assert_eq!(prop.children[1].trimmed(), "A & B A<c>");
        assert_eq!(prop.children[2].attr("attr"), Some("1 < 2"));
        assert_eq!(el.depth(), 3);
        assert_eq!(el.count(), 6);
    }

    #[test]
    fn rejects() {
        let cases: [(&[u8], XmlError); 5] = [
            (b"<!DOCTYPE x [<!ENTITY a \"b\">]><x>&a;</x>", XmlError::Dtd),
            (b"<x>&a;</x>", XmlError::Reference("a".into())),
            (b"", XmlError::NoRoot),
            (
                b"<a><b><c><d><e><f><g><h><i/></h></g></f></e></d></c></b></a>",
                XmlError::Depth(8),
            ),
            (b"\xff<x/>", XmlError::Utf8),
        ];
        for (input, want) in cases {
            assert_eq!(parse(input, LIMITS), Err(want), "{input:?}");
        }
        let many = format!("<x>{}</x>", "<y/>".repeat(50));
        assert_eq!(parse(many.as_bytes(), LIMITS), Err(XmlError::Elements(50)));
        let bad: [&[u8]; 2] = [b"<x><y></x>", b"<x/><y/>"];
        for bad in bad {
            assert!(
                matches!(parse(bad, LIMITS), Err(XmlError::Syntax(_))),
                "{bad:?}"
            );
        }
        assert!(matches!(
            parse(b"<p:x/>", LIMITS),
            Err(XmlError::Reference(_))
        ));
    }

    #[test]
    fn writes() {
        let mut ms = Multistatus::new();
        ms.props(
            "/dav/a b/",
            &[
                (
                    StatusCode::OK,
                    vec![
                        (
                            PropName::new(DAV, "displayname"),
                            text("A & B").into_owned(),
                        ),
                        (PropName::new(CS, "getctag"), "7".into()),
                    ],
                ),
                (
                    StatusCode::NOT_FOUND,
                    vec![
                        (PropName::new("urn:z", "y"), String::new()),
                        (PropName::new("", "plain"), String::new()),
                    ],
                ),
            ],
        );
        ms.status(
            "/dav/x",
            StatusCode::INSUFFICIENT_STORAGE,
            Some("<d:number-of-matches-within-limits/>"),
        );
        ms.sync_token("urn:thepost:sync:7");
        let body = ms.finish();
        let tail = body.split_once("ical/\">").expect("root").1;
        assert_eq!(
            tail,
            concat!(
                "<d:response><d:href>/dav/a b/</d:href><d:propstat><d:prop>",
                "<d:displayname>A &amp; B</d:displayname><cs:getctag>7</cs:getctag></d:prop>",
                "<d:status>HTTP/1.1 200 OK</d:status></d:propstat><d:propstat><d:prop>",
                "<x:y xmlns:x=\"urn:z\"/><plain/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status>",
                "</d:propstat></d:response><d:response><d:href>/dav/x</d:href>",
                "<d:status>HTTP/1.1 507 Insufficient Storage</d:status>",
                "<d:error><d:number-of-matches-within-limits/></d:error></d:response>",
                "<d:sync-token>urn:thepost:sync:7</d:sync-token></d:multistatus>"
            )
        );
        assert!(body.starts_with(HEADER));
        let again = parse(body.as_bytes(), LIMITS).expect("ответ разбирается");
        assert!(again.is(DAV, "multistatus"));
    }
}
