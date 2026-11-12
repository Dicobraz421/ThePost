//! Отчёты CalDAV по календарю: `calendar-query` (RFC 4791 §7.8), `calendar-multiget` (§7.9), `free-busy-query`
//! (§7.10).
//!
//! `calendar-query`: фильтр `comp-filter` → `prop-filter` → `param-filter` с `is-not-defined`, `text-match`
//! (подстрока; сравнения `i;ascii-casemap` — по умолчанию, `i;unicode-casemap`, `i;octet`; `negate-condition`) и
//! `time-range`. Кандидаты `time-range` — из индекса хранилища (`in_range`), точная проверка — по развёрнутым
//! calcard вхождениям (повторы, `EXDATE`, переопределения `RECURRENCE-ID`; не больше `dav.max_instances` вхождений
//! объекта). Время без пояса и даты «весь день» считаются в поясе календаря (`calendar-timezone`) или поясе
//! запроса (`timezone`), иначе в UTC. `time-range` поддерживается у компонентов VEVENT, VTODO, VJOURNAL (задача
//! без дат подходит любому интервалу, §9.9) и у свойств с датой; у вложенных компонентов (VALARM) — 403
//! `supported-filter`. Неверный фильтр — 403 `valid-filter`, неизвестное сравнение — 403 `supported-collation`.
//!
//! `calendar-data`: `expand` — вхождения интервала отдельными компонентами во времени UTC без правил повтора,
//! `limit-recurrence-set` — основной компонент и переопределения, попавшие в интервал; выбор отдельных свойств
//! (`comp`, `prop`) не применяется — возвращается объект целиком.
//!
//! `free-busy-query`: VFREEBUSY по событиям календаря в интервале: `TRANSP:TRANSPARENT` и `STATUS:CANCELLED` не
//! занимают время, `STATUS:TENTATIVE` — `BUSY-TENTATIVE`, остальное — `BUSY`; пересекающиеся периоды слиты.
//!
//! Ответы `multistatus` ограничены `dav.max_response_items` (остаток — `507`
//! `number-of-matches-within-limits`).

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::str::FromStr;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use calcard::common::PartialDateTime;
use calcard::common::timezone::Tz;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarEntry, ICalendarFreeBusyType,
    ICalendarParameter, ICalendarPeriod, ICalendarProperty, ICalendarStatus, ICalendarTransparency,
    ICalendarValue, ICalendarValueType,
};
use time::{Date, Month, OffsetDateTime};
use tp_contracts::Result;
use tp_contracts::dav::{Collection, CollectionKind, Component, DavObject, TimeRange};

use crate::path::{self, Target};
use crate::props::{self, Res, Want, content_type};
use crate::server::{Cx, multistatus, precondition, status};
use crate::xml::{CALDAV, DAV, Element, Multistatus};

/// Интервал времени запроса, секунды Unix: `[start, end)`; открытые границы — `i64::MIN`, `i64::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Range {
    pub start: i64,
    pub end: i64,
}

/// Время UTC в записи iCalendar (`20260105T100000Z`).
fn utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<u8>().ok();
    let year = s.get(0..4)?.parse::<i32>().ok()?;
    let month = Month::try_from(n(4..6)?).ok()?;
    let date = Date::from_calendar_date(year, month, n(6..8)?).ok()?;
    let dt = date.with_hms(n(9..11)?, n(11..13)?, n(13..15)?).ok()?;
    Some(dt.assume_utc().unix_timestamp())
}

impl Range {
    /// Атрибуты `start` и `end` элемента (`time-range`, `expand`, `limit-recurrence-set`); `both` — обе границы
    /// обязательны. Нет границ, неверная запись или `end ≤ start` — `None`.
    pub(crate) fn parse(el: &Element, both: bool) -> Option<Range> {
        let (s, e) = (el.attr("start"), el.attr("end"));
        if (s.is_none() && e.is_none()) || (both && (s.is_none() || e.is_none())) {
            return None;
        }
        let start = s.map(utc).unwrap_or(Some(i64::MIN))?;
        let end = e.map(utc).unwrap_or(Some(i64::MAX))?;
        (start < end).then_some(Range { start, end })
    }

    /// Интервал `[s, e)` пересекает запрос; вхождение нулевой длины — по его началу (RFC 4791 §9.9).
    pub(crate) fn overlaps(&self, s: i64, e: i64) -> bool {
        s < self.end && (e > self.start || (e == s && s >= self.start))
    }

    fn contains(&self, t: i64) -> bool {
        self.start <= t && t < self.end
    }

    /// Интервал для выбора кандидатов в хранилище: шире запроса на сутки с каждой стороны — индекс считает время
    /// без пояса и даты «весь день» как UTC, а точная проверка — в поясе календаря (сдвиг до 14 часов).
    pub(crate) fn store(&self) -> TimeRange {
        const SKEW: i64 = 86_400;
        let at = |t: i64| OffsetDateTime::from_unix_timestamp(t).ok();
        TimeRange {
            start: (self.start != i64::MIN)
                .then(|| at(self.start.saturating_sub(SKEW)))
                .flatten(),
            end: (self.end != i64::MAX)
                .then(|| at(self.end.saturating_add(SKEW)))
                .flatten(),
        }
    }
}

/// Ошибка фильтра.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilterError {
    /// Неверная запись (`valid-filter`).
    Invalid,
    /// Неизвестное сравнение (`supported-collation`).
    Collation,
    /// Фильтр не поддерживается (`supported-filter`).
    Unsupported,
}

/// Сравнение `text-match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Collation {
    Octet,
    Ascii,
    Unicode,
}

/// Вид совпадения `text-match` (`match-type`, RFC 6352 §10.5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchType {
    Equals,
    Contains,
    StartsWith,
    EndsWith,
}

/// `text-match`: CalDAV — подстрока, CardDAV — по `match-type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextMatch {
    /// Искомое, приведённое по сравнению.
    needle: String,
    collation: Collation,
    kind: MatchType,
    negate: bool,
}

impl TextMatch {
    /// `text-match` CalDAV (RFC 4791 §9.7.5): сравнение по умолчанию — `i;ascii-casemap`, подстрока.
    fn parse(el: &Element) -> std::result::Result<TextMatch, FilterError> {
        Self::with(el, Collation::Ascii, MatchType::Contains)
    }

    /// `text-match` CardDAV (RFC 6352 §10.5.4): сравнение по умолчанию — `i;unicode-casemap`, вид — `match-type`
    /// (`equals`, `contains` — по умолчанию, `starts-with`, `ends-with`).
    pub(crate) fn parse_card(el: &Element) -> std::result::Result<TextMatch, FilterError> {
        let kind = match el.attr("match-type") {
            None | Some("contains") => MatchType::Contains,
            Some("equals") => MatchType::Equals,
            Some("starts-with") => MatchType::StartsWith,
            Some("ends-with") => MatchType::EndsWith,
            Some(_) => return Err(FilterError::Invalid),
        };
        Self::with(el, Collation::Unicode, kind)
    }

    fn with(
        el: &Element,
        default: Collation,
        kind: MatchType,
    ) -> std::result::Result<TextMatch, FilterError> {
        let collation = match el.attr("collation") {
            None => default,
            Some("i;ascii-casemap") => Collation::Ascii,
            Some("i;unicode-casemap") => Collation::Unicode,
            Some("i;octet") => Collation::Octet,
            Some(_) => return Err(FilterError::Collation),
        };
        let negate = match el.attr("negate-condition") {
            None | Some("no") => false,
            Some("yes") => true,
            Some(_) => return Err(FilterError::Invalid),
        };
        let mut m = TextMatch {
            needle: String::new(),
            collation,
            kind,
            negate,
        };
        m.needle = m.fold(&el.text).into_owned();
        Ok(m)
    }

    fn fold<'a>(&self, s: &'a str) -> Cow<'a, str> {
        match self.collation {
            Collation::Octet => Cow::Borrowed(s),
            Collation::Ascii => Cow::Owned(s.to_ascii_lowercase()),
            Collation::Unicode => Cow::Owned(s.to_lowercase()),
        }
    }

    /// Искомое для поиска контактов по индексу (CTR-54, `DavStore::find_cards`): подстрока, которую содержит
    /// поисковая строка ([`tp_contracts::dav::CardSearch`]: значение в нижнем регистре без пробелов по краям) каждого
    /// значения, совпавшего с условием, — при любом сравнении и виде совпадения. `None` — индекс не сужает
    /// кандидатов: условие с `negate-condition`, пустое искомое, искомое с `;` (совпадение с частями через `;`) или
    /// с управляющими символами (в индексе они заменены пробелом).
    pub(crate) fn index_needle(&self) -> Option<String> {
        let needle = self.needle.trim().to_lowercase();
        let usable = !self.negate
            && !needle.is_empty()
            && !needle.contains(';')
            && !needle.chars().any(char::is_control);
        usable.then_some(needle)
    }

    /// Хотя бы одно значение совпадает с искомым (с учётом `negate-condition`).
    pub(crate) fn matches<'a>(&self, values: impl IntoIterator<Item = &'a str>) -> bool {
        let needle = self.needle.as_str();
        let found = values.into_iter().any(|v| {
            let v = self.fold(v);
            match self.kind {
                MatchType::Equals => v == needle,
                MatchType::Contains => v.contains(needle),
                MatchType::StartsWith => v.starts_with(needle),
                MatchType::EndsWith => v.ends_with(needle),
            }
        });
        found != self.negate
    }
}

/// `param-filter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParamFilter {
    name: String,
    not_defined: bool,
    text: Option<TextMatch>,
}

/// `prop-filter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PropFilter {
    name: String,
    not_defined: bool,
    range: Option<Range>,
    text: Option<TextMatch>,
    params: Vec<ParamFilter>,
}

/// `comp-filter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompFilter {
    name: String,
    not_defined: bool,
    range: Option<Range>,
    props: Vec<PropFilter>,
    comps: Vec<CompFilter>,
}

fn name_of(el: &Element) -> std::result::Result<String, FilterError> {
    el.attr("name")
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .ok_or(FilterError::Invalid)
}

fn range_of(el: &Element) -> std::result::Result<Range, FilterError> {
    Range::parse(el, false).ok_or(FilterError::Invalid)
}

impl ParamFilter {
    fn parse(el: &Element) -> std::result::Result<ParamFilter, FilterError> {
        Ok(ParamFilter {
            name: name_of(el)?,
            not_defined: el.child(CALDAV, "is-not-defined").is_some(),
            text: el
                .child(CALDAV, "text-match")
                .map(TextMatch::parse)
                .transpose()?,
        })
    }
}

impl PropFilter {
    fn parse(el: &Element) -> std::result::Result<PropFilter, FilterError> {
        Ok(PropFilter {
            name: name_of(el)?,
            not_defined: el.child(CALDAV, "is-not-defined").is_some(),
            range: el.child(CALDAV, "time-range").map(range_of).transpose()?,
            text: el
                .child(CALDAV, "text-match")
                .map(TextMatch::parse)
                .transpose()?,
            params: el
                .children
                .iter()
                .filter(|c| c.is(CALDAV, "param-filter"))
                .map(ParamFilter::parse)
                .collect::<std::result::Result<_, _>>()?,
        })
    }
}

/// Компоненты, у которых поддерживается `time-range`.
const RANGED: [&str; 3] = ["VEVENT", "VTODO", "VJOURNAL"];

impl CompFilter {
    fn parse_at(el: &Element, depth: usize) -> std::result::Result<CompFilter, FilterError> {
        let f = CompFilter {
            name: name_of(el)?,
            not_defined: el.child(CALDAV, "is-not-defined").is_some(),
            range: el.child(CALDAV, "time-range").map(range_of).transpose()?,
            props: el
                .children
                .iter()
                .filter(|c| c.is(CALDAV, "prop-filter"))
                .map(PropFilter::parse)
                .collect::<std::result::Result<_, _>>()?,
            comps: el
                .children
                .iter()
                .filter(|c| c.is(CALDAV, "comp-filter"))
                .map(|c| CompFilter::parse_at(c, depth + 1))
                .collect::<std::result::Result<_, _>>()?,
        };
        if depth == 0 && !f.name.eq_ignore_ascii_case("VCALENDAR") {
            return Err(FilterError::Invalid);
        }
        if f.range.is_some()
            && !(depth == 1 && RANGED.iter().any(|n| n.eq_ignore_ascii_case(&f.name)))
        {
            return Err(FilterError::Unsupported);
        }
        Ok(f)
    }

    /// Фильтр из элемента `filter` запроса (`comp-filter name="VCALENDAR"`).
    pub(crate) fn parse(filter: &Element) -> std::result::Result<CompFilter, FilterError> {
        let top = filter
            .child(CALDAV, "comp-filter")
            .ok_or(FilterError::Invalid)?;
        CompFilter::parse_at(top, 0)
    }

    /// Интервал для выбора кандидатов: `time-range` единственного фильтра компонента; иначе — все объекты.
    fn candidates(&self) -> TimeRange {
        match self.comps.as_slice() {
            // Задачи — все: задача без срока подходит интервалу и вне своего интервала индекса (§9.9).
            [one] if !one.name.eq_ignore_ascii_case("VTODO") => {
                one.range.map(|r| r.store()).unwrap_or_default()
            }
            _ => TimeRange::default(),
        }
    }

    /// Фильтр только по виду компонента (или без условий): решается по индексу, без разбора тела.
    /// `Some(None)` — подходят все объекты.
    fn by_component(&self) -> Option<Option<Component>> {
        if self.not_defined || !self.props.is_empty() {
            return None;
        }
        match self.comps.as_slice() {
            [] => Some(None),
            [c] if !c.not_defined
                && c.range.is_none()
                && c.props.is_empty()
                && c.comps.is_empty() =>
            {
                Component::from_str(&c.name)
                    .ok()
                    .filter(|x| x.fits(CollectionKind::Calendar))
                    .map(Some)
            }
            _ => None,
        }
    }
}

/// Развёрнутое вхождение.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Instance {
    /// Номер компонента в объекте.
    comp: u32,
    /// Начало и конец, UTC.
    start: i64,
    end: i64,
    /// Начало в местном времени вхождения (как секунды Unix) — для дат «весь день».
    local: i64,
}

#[derive(Debug, Default)]
struct Expanded {
    instances: Vec<Instance>,
    /// Компоненты с ошибкой дат.
    errors: Vec<u32>,
}

/// Разобранный объект календаря с развёрткой по требованию.
pub(crate) struct Object<'a> {
    ical: &'a ICalendar,
    tz: Tz,
    max_instances: usize,
    expanded: OnceCell<Expanded>,
}

fn is_type(c: &ICalendarComponent, name: &str) -> bool {
    c.component_type.as_str().eq_ignore_ascii_case(name)
}

impl<'a> Object<'a> {
    /// Объект с поясом по умолчанию `tz` (время без пояса) и лимитом развёртки.
    pub(crate) fn new(ical: &'a ICalendar, tz: Tz, max_instances: usize) -> Self {
        Self {
            ical,
            tz,
            max_instances,
            expanded: OnceCell::new(),
        }
    }

    fn expanded(&self) -> &Expanded {
        self.expanded.get_or_init(|| {
            let exp = self.ical.expand_dates(self.tz, self.max_instances);
            Expanded {
                instances: exp
                    .events
                    .iter()
                    .map(|e| {
                        let (start, end) = e.timestamps();
                        Instance {
                            comp: e.comp_id,
                            start,
                            end: end.max(start),
                            local: e.start.naive_local().and_utc().timestamp(),
                        }
                    })
                    .collect(),
                errors: exp.errors.iter().map(|e| e.comp_id).collect(),
            }
        })
    }

    fn component(&self, id: u32) -> Option<&'a ICalendarComponent> {
        self.ical.components.get(usize::try_from(id).ok()?)
    }

    /// Объект подходит фильтру (`comp-filter name="VCALENDAR"`).
    pub(crate) fn matches(&self, f: &CompFilter) -> bool {
        let Some(root) = self.ical.components.first() else {
            return false;
        };
        if !is_type(root, &f.name) {
            return f.not_defined;
        }
        !f.not_defined && self.body_matches(root, f)
    }

    fn body_matches(&self, comp: &ICalendarComponent, f: &CompFilter) -> bool {
        f.props.iter().all(|p| self.prop_matches(comp, p))
            && f.comps.iter().all(|c| self.child_matches(comp, c))
    }

    fn child_matches(&self, parent: &ICalendarComponent, f: &CompFilter) -> bool {
        let kids: Vec<&ICalendarComponent> = parent
            .component_ids
            .iter()
            .filter_map(|id| self.component(*id))
            .filter(|c| is_type(c, &f.name))
            .collect();
        if f.not_defined {
            return kids.is_empty();
        }
        if kids.is_empty() || f.range.is_some_and(|r| !self.in_range(&f.name, r)) {
            return false;
        }
        kids.into_iter().any(|k| self.body_matches(k, f))
    }

    /// Задача без повторов пересекает интервал (RFC 4791 §9.9, таблица VTODO).
    fn todo_in_range(&self, comp: &ICalendarComponent, r: Range) -> bool {
        use ICalendarProperty as P;
        let at = |p: P| comp.property(&p).and_then(|e| self.entry_time(e));
        let duration = comp
            .property(&P::Duration)
            .and_then(|e| match e.values.first() {
                Some(ICalendarValue::Duration(d)) => Some(d.as_seconds()),
                _ => None,
            });
        match (
            at(P::Dtstart),
            duration,
            at(P::Due),
            at(P::Completed),
            at(P::Created),
        ) {
            (Some(s), Some(d), ..) => r.start <= s + d && (r.end > s || r.end >= s + d),
            (Some(s), None, Some(due), ..) => {
                (r.start < due || r.start <= s) && (r.end > s || r.end >= due)
            }
            (Some(s), None, None, ..) => r.start <= s && r.end > s,
            (None, _, Some(due), ..) => r.start < due && r.end >= due,
            (None, _, None, Some(done), Some(created)) => {
                (r.start <= created || r.start <= done) && (r.end >= created || r.end >= done)
            }
            (None, _, None, Some(done), None) => r.start <= done && r.end >= done,
            (None, _, None, None, Some(created)) => r.end > created,
            (None, _, None, None, None) => true,
        }
    }

    /// Вхождения компонентов вида `name` пересекают интервал.
    fn in_range(&self, name: &str, r: Range) -> bool {
        if name.eq_ignore_ascii_case("VTODO") {
            let todos: Vec<&ICalendarComponent> = self
                .ical
                .components
                .iter()
                .filter(|c| c.component_type == ICalendarComponentType::VTodo)
                .collect();
            if !todos.iter().any(|c| c.is_recurrent_or_override()) {
                return todos.into_iter().any(|c| self.todo_in_range(c, r));
            }
        }
        let exp = self.expanded();
        let of_type = |id: u32| self.component(id).is_some_and(|c| is_type(c, name));
        let mut dated = false;
        for i in exp.instances.iter().filter(|i| of_type(i.comp)) {
            if r.overlaps(i.start, i.end) {
                return true;
            }
            dated = true;
        }
        // Задача без дат подходит любому интервалу (RFC 4791 §9.9).
        name.eq_ignore_ascii_case("VTODO") && !dated && !exp.errors.iter().any(|id| of_type(*id))
    }

    fn prop_matches(&self, comp: &ICalendarComponent, f: &PropFilter) -> bool {
        let mut entries = comp
            .entries
            .iter()
            .filter(|e| e.name.as_str().eq_ignore_ascii_case(&f.name))
            .peekable();
        if f.not_defined {
            return entries.peek().is_none();
        }
        entries.any(|e| {
            f.range
                .is_none_or(|r| self.entry_time(e).is_some_and(|t| r.contains(t)))
                && f.text
                    .as_ref()
                    .is_none_or(|t| t.matches(entry_texts(e).iter().map(AsRef::as_ref)))
                && f.params.iter().all(|p| param_matches(e, p))
        })
    }

    /// Время значения свойства с датой, UTC.
    fn entry_time(&self, e: &ICalendarEntry) -> Option<i64> {
        let dt = e.values.first()?.as_partial_date_time()?;
        let tz = self
            .ical
            .build_tz_resolver()
            .with_default(self.tz)
            .resolve_or_default(e.tz_id());
        Some(dt.to_date_time_with_tz(tz)?.timestamp())
    }
}

fn param_matches(e: &ICalendarEntry, f: &ParamFilter) -> bool {
    let mut params = e
        .params
        .iter()
        .filter(|p| p.name.as_str().eq_ignore_ascii_case(&f.name))
        .peekable();
    if f.not_defined {
        return params.peek().is_none();
    }
    params.any(|p| f.text.as_ref().is_none_or(|t| t.matches(p.value.as_text())))
}

/// Текстовые значения свойства; у свойства без текстовых значений (дата, длительность, число) — его запись
/// iCalendar.
fn entry_texts(e: &ICalendarEntry) -> Vec<Cow<'_, str>> {
    let texts: Vec<Cow<'_, str>> = e
        .values
        .iter()
        .filter_map(ICalendarValue::as_text)
        .map(Cow::Borrowed)
        .collect();
    if !texts.is_empty() {
        return texts;
    }
    let mut line = String::new();
    if e.write_to(&mut line).is_err() {
        return Vec::new();
    }
    let line = line.replace("\r\n ", "").replace("\r\n\t", "");
    let mut quoted = false;
    let colon = line.char_indices().find_map(|(i, ch)| {
        if ch == '"' {
            quoted = !quoted;
        }
        (ch == ':' && !quoted).then_some(i)
    });
    match colon {
        Some(i) => vec![Cow::Owned(line[i + 1..].trim_end().to_owned())],
        None => Vec::new(),
    }
}

/// Параметры `calendar-data` запроса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct DataOptions {
    expand: Option<Range>,
    limit: Option<Range>,
}

impl DataOptions {
    /// Из элемента `prop` отчёта; неверный интервал — `None`.
    pub(crate) fn parse(prop: Option<&Element>) -> Option<DataOptions> {
        let Some(data) = prop.and_then(|p| p.child(CALDAV, "calendar-data")) else {
            return Some(DataOptions::default());
        };
        let range = |name: &str| match data.child(CALDAV, name) {
            Some(el) => Range::parse(el, true).map(Some),
            None => Some(None),
        };
        Some(DataOptions {
            expand: range("expand")?,
            limit: range("limit-recurrence-set")?,
        })
    }

    fn is_plain(&self) -> bool {
        self.expand.is_none() && self.limit.is_none()
    }

    /// Тело объекта для ответа.
    fn render<'b>(&self, body: &'b str, obj: &Object<'_>) -> Cow<'b, str> {
        if let Some(r) = self.expand {
            return Cow::Owned(expand(obj, r));
        }
        if let Some(r) = self.limit {
            return limit(body, obj, r);
        }
        Cow::Borrowed(body)
    }
}

/// Копия компонента `id` со вложенными в `out`; номер копии.
fn copy_tree(src: &ICalendar, id: u32, out: &mut Vec<ICalendarComponent>) -> Option<u32> {
    let comp = src.components.get(usize::try_from(id).ok()?)?;
    let mut copy = ICalendarComponent {
        component_type: comp.component_type.clone(),
        entries: comp.entries.clone(),
        component_ids: Vec::new(),
    };
    for child in &comp.component_ids {
        if let Some(n) = copy_tree(src, *child, out) {
            copy.component_ids.push(n);
        }
    }
    let n = u32::try_from(out.len()).ok()?;
    out.push(copy);
    Some(n)
}

/// Свойство с датой: «весь день» — `VALUE=DATE` по местной дате, иначе время UTC.
fn date_entry(name: ICalendarProperty, all_day: bool, utc: i64, local: i64) -> ICalendarEntry {
    if all_day {
        ICalendarEntry::new(name)
            .with_param(ICalendarParameter::value(ICalendarValueType::Date))
            .with_value(PartialDateTime::from_date_timestamp(local))
    } else {
        ICalendarEntry::new(name).with_value(PartialDateTime::from_utc_timestamp(utc))
    }
}

/// `expand`: вхождения интервала отдельными компонентами без правил повтора (RFC 4791 §9.6.5).
fn expand(obj: &Object<'_>, r: Range) -> String {
    use ICalendarProperty as P;
    let src = obj.ical;
    let Some(root) = src.components.first() else {
        return String::new();
    };
    let mut out = vec![ICalendarComponent {
        component_type: root.component_type.clone(),
        entries: root.entries.clone(),
        component_ids: Vec::new(),
    }];
    let recurring = src
        .components
        .iter()
        .any(ICalendarComponent::is_recurrent_or_override);
    let mut instances: Vec<&Instance> = obj
        .expanded()
        .instances
        .iter()
        .filter(|i| r.overlaps(i.start, i.end))
        .collect();
    instances.sort_by_key(|i| (i.start, i.comp));
    for i in instances {
        let Some(comp) = obj.component(i.comp) else {
            continue;
        };
        let date_of = |p: &P| {
            comp.property(p)
                .and_then(|e| e.values.first())
                .and_then(ICalendarValue::as_partial_date_time)
        };
        let all_day = date_of(&P::Dtstart)
            .or_else(|| date_of(&P::RecurrenceId))
            .is_some_and(|d| !d.has_time());
        let mut copy = ICalendarComponent::new(comp.component_type.clone());
        copy.entries = comp
            .entries
            .iter()
            .filter(|e| {
                !matches!(
                    e.name,
                    P::Rrule
                        | P::Rdate
                        | P::Exdate
                        | P::Exrule
                        | P::RecurrenceId
                        | P::Dtstart
                        | P::Dtend
                        | P::Duration
                        | P::Due
                )
            })
            .cloned()
            .collect();
        copy.entries
            .push(date_entry(P::Dtstart, all_day, i.start, i.local));
        let len = i.end - i.start;
        let end = match comp.component_type {
            ICalendarComponentType::VEvent => Some(P::Dtend),
            ICalendarComponentType::VTodo
                if comp.has_property(&P::Due) || comp.has_property(&P::Duration) =>
            {
                Some(P::Due)
            }
            _ => None,
        };
        if let Some(end) = end
            && len > 0
        {
            copy.entries
                .push(date_entry(end, all_day, i.end, i.local + len));
        }
        if recurring {
            let rid = comp
                .property(&P::RecurrenceId)
                .and_then(|e| obj.entry_time(e))
                .unwrap_or(i.start);
            // Местная дата переопределения «весь день» совпадает с датой самого RECURRENCE-ID.
            let rid_local = date_of(&P::RecurrenceId)
                .and_then(|d| d.to_date_time_with_tz(Tz::UTC))
                .map_or(i.local, |d| d.timestamp());
            copy.entries
                .push(date_entry(P::RecurrenceId, all_day, rid, rid_local));
        }
        for child in &comp.component_ids {
            if let Some(n) = copy_tree(src, *child, &mut out) {
                copy.component_ids.push(n);
            }
        }
        if let Ok(n) = u32::try_from(out.len()) {
            out.push(copy);
            out[0].component_ids.push(n);
        }
    }
    ICalendar { components: out }.to_string()
}

/// `limit-recurrence-set`: без переопределений вне интервала (RFC 4791 §9.6.6).
fn limit<'b>(body: &'b str, obj: &Object<'_>, r: Range) -> Cow<'b, str> {
    let exp = obj.expanded();
    let drop: Vec<u32> = (0u32..)
        .zip(&obj.ical.components)
        .filter(|(id, c)| {
            c.is_recurrence_override()
                && !exp
                    .instances
                    .iter()
                    .any(|i| i.comp == *id && r.overlaps(i.start, i.end))
        })
        .map(|(id, _)| id)
        .collect();
    if drop.is_empty() {
        return Cow::Borrowed(body);
    }
    let mut copy = obj.ical.clone();
    copy.remove_component_ids(&drop);
    Cow::Owned(copy.to_string())
}

/// Пояс для времени без пояса: календаря (`calendar-timezone`) или UTC.
fn collection_tz(c: &Collection) -> Tz {
    c.timezone
        .as_deref()
        .and_then(|t| Tz::from_str(t).ok())
        .unwrap_or(Tz::UTC)
}

/// Пояс запроса (`timezone`: VCALENDAR с VTIMEZONE) или календаря.
fn request_tz(root: &Element, c: &Collection) -> Tz {
    root.child(CALDAV, "timezone")
        .and_then(|t| ICalendar::parse(&t.text).ok())
        .and_then(|ical| {
            ical.timezones()
                .find_map(ICalendarComponent::timezone)
                .map(|(_, tz)| tz)
        })
        .unwrap_or_else(|| collection_tz(c))
}

/// Запрошенные свойства отчёта: `prop`, `allprop` или `propname`; без них — только ссылки.
pub(crate) fn want_of(root: &Element) -> Want {
    if let Some(p) = root.child(DAV, "prop") {
        Want::Props(props::keys(p))
    } else if root.child(DAV, "allprop").is_some() {
        Want::All(Vec::new())
    } else if root.child(DAV, "propname").is_some() {
        Want::Names
    } else {
        Want::Props(Vec::new())
    }
}

fn filter_error(e: FilterError) -> Response {
    let inner = match e {
        FilterError::Invalid => "<c:valid-filter/>",
        FilterError::Collation => "<c:supported-collation/>",
        FilterError::Unsupported => "<c:supported-filter/>",
    };
    precondition(StatusCode::FORBIDDEN, inner)
}

/// Отчёт не поддерживается ресурсом.
pub(crate) fn unsupported() -> Response {
    precondition(StatusCode::FORBIDDEN, "<d:supported-report/>")
}

/// Отчёты CalDAV (корневой элемент `root` в пространстве CalDAV).
pub(crate) async fn report(cx: &Cx<'_>, target: &Target, root: &Element) -> Result<Response> {
    let Target::Collection(CollectionKind::Calendar, _, name) = target else {
        return Ok(unsupported());
    };
    let kind = CollectionKind::Calendar;
    let Some(c) = cx.store().collection_by_name(cx.scope, kind, name).await? else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    match root.name.as_str() {
        "calendar-query" => calendar_query(cx, &c, root).await,
        "calendar-multiget" => multiget(cx, &c, root).await,
        "free-busy-query" => free_busy(cx, &c, root).await,
        _ => Ok(unsupported()),
    }
}

/// Объект в ответ: свойства `want`, тело — по параметрам `calendar-data`.
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
        tag: o.index.schedule_tag.as_deref(),
    };
    props::emit(cx, ms, &res, want);
}

async fn calendar_query(cx: &Cx<'_>, c: &Collection, root: &Element) -> Result<Response> {
    let Some(filter) = root.child(CALDAV, "filter") else {
        return Ok(filter_error(FilterError::Invalid));
    };
    let filter = match CompFilter::parse(filter) {
        Ok(f) => f,
        Err(e) => return Ok(filter_error(e)),
    };
    let Some(opts) = DataOptions::parse(root.child(DAV, "prop")) else {
        return Ok(status(StatusCode::BAD_REQUEST));
    };
    let want = want_of(root);
    let need_body = want.needs_body();
    let tz = request_tz(root, c);
    let objects = cx
        .store()
        .in_range(cx.scope, c.id, filter.candidates())
        .await?;
    let by_component = filter.by_component();
    let max = cx.dav.cfg.max_response_items;
    let mut ms = Multistatus::new();
    let mut sent = 0usize;
    let mut more = false;
    for o in &objects {
        let plain = by_component.filter(|_| !need_body || opts.is_plain());
        let ical = match plain {
            Some(component) => {
                if component.is_some_and(|x| x != o.index.component) {
                    continue;
                }
                None
            }
            None => match ICalendar::parse(&o.body) {
                Ok(ical) => Some(ical),
                Err(_) => continue,
            },
        };
        let obj = ical
            .as_ref()
            .map(|i| Object::new(i, tz, cx.dav.cfg.max_instances));
        if let Some(obj) = &obj
            && !obj.matches(&filter)
        {
            continue;
        }
        if sent == max {
            more = true;
            break;
        }
        sent += 1;
        let body = need_body.then(|| match &obj {
            Some(obj) => opts.render(&o.body, obj),
            None => Cow::Borrowed(o.body.as_str()),
        });
        emit(cx, &mut ms, c, o, &want, body.as_deref());
    }
    if more {
        props::truncated(&mut ms, &path::collection(c.kind, cx.login(), &c.name));
    }
    Ok(multistatus(ms))
}

/// Имя объекта коллекции `c` по ссылке `href` (путь или полный URL); чужая или неверная ссылка — `None`.
pub(crate) fn object_name(cx: &Cx<'_>, c: &Collection, href: &str) -> Option<String> {
    let path = match href.split_once("://") {
        Some((_, rest)) => &rest[rest.find('/')?..],
        None => href,
    };
    match Target::parse(path)? {
        Target::Object(kind, login, coll, name)
            if kind == c.kind && login.eq_ignore_ascii_case(cx.login()) && coll == c.name =>
        {
            Some(name)
        }
        _ => None,
    }
}

/// `calendar-multiget`: объекты по ссылкам в порядке запроса; отсутствующие и чужие — 404 в ответе.
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
    let Some(opts) = DataOptions::parse(root.child(DAV, "prop")) else {
        return Ok(status(StatusCode::BAD_REQUEST));
    };
    let want = want_of(root);
    let need_body = want.needs_body();
    let tz = collection_tz(c);
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
        let body = need_body.then(|| {
            if opts.is_plain() {
                return Cow::Borrowed(o.body.as_str());
            }
            match ICalendar::parse(&o.body) {
                Ok(ical) => {
                    let obj = Object::new(&ical, tz, cx.dav.cfg.max_instances);
                    Cow::Owned(opts.render(&o.body, &obj).into_owned())
                }
                Err(_) => Cow::Borrowed(o.body.as_str()),
            }
        });
        emit(cx, &mut ms, c, o, &want, body.as_deref());
    }
    Ok(multistatus(ms))
}

/// Сливает пересекающиеся и смежные периоды.
fn merge(mut periods: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    periods.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (s, e) in periods {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

async fn free_busy(cx: &Cx<'_>, c: &Collection, root: &Element) -> Result<Response> {
    let Some(r) = root
        .child(CALDAV, "time-range")
        .and_then(|t| Range::parse(t, true))
    else {
        return Ok(filter_error(FilterError::Invalid));
    };
    let tz = collection_tz(c);
    let objects = cx.store().in_range(cx.scope, c.id, r.store()).await?;
    let mut busy: HashMap<bool, Vec<(i64, i64)>> = HashMap::new();
    for o in objects
        .iter()
        .filter(|o| o.index.component == Component::Event)
    {
        let Ok(ical) = ICalendar::parse(&o.body) else {
            continue;
        };
        let obj = Object::new(&ical, tz, cx.dav.cfg.max_instances);
        for i in &obj.expanded().instances {
            let Some(comp) = obj.component(i.comp) else {
                continue;
            };
            if comp.component_type != ICalendarComponentType::VEvent
                || !r.overlaps(i.start, i.end)
                || comp.transparency() == Some(&ICalendarTransparency::Transparent)
                || comp.status() == Some(&ICalendarStatus::Cancelled)
            {
                continue;
            }
            let (s, e) = (i.start.max(r.start), i.end.min(r.end));
            if s < e {
                let tentative = comp.status() == Some(&ICalendarStatus::Tentative);
                busy.entry(tentative).or_default().push((s, e));
            }
        }
    }
    let mut fb = ICalendarComponent::new(ICalendarComponentType::VFreebusy);
    fb.add_dtstamp(PartialDateTime::now());
    fb.add_property(
        ICalendarProperty::Dtstart,
        PartialDateTime::from_utc_timestamp(r.start),
    );
    fb.add_property(
        ICalendarProperty::Dtend,
        PartialDateTime::from_utc_timestamp(r.end),
    );
    for (tentative, fbtype) in [
        (false, ICalendarFreeBusyType::Busy),
        (true, ICalendarFreeBusyType::BusyTentative),
    ] {
        let periods = merge(busy.remove(&tentative).unwrap_or_default());
        if periods.is_empty() {
            continue;
        }
        let values = periods
            .into_iter()
            .map(|(s, e)| {
                ICalendarValue::Period(ICalendarPeriod::Range {
                    start: PartialDateTime::from_utc_timestamp(s),
                    end: PartialDateTime::from_utc_timestamp(e),
                })
            })
            .collect();
        fb.entries.push(
            ICalendarEntry::new(ICalendarProperty::Freebusy)
                .with_param(ICalendarParameter::fbtype(fbtype))
                .with_values(values),
        );
    }
    let mut ical = props::vcalendar();
    ical.components.push(fb);
    ical.components[0].component_ids.push(1);
    Ok((
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(content_type(CollectionKind::Calendar)),
        )],
        ical.to_string(),
    )
        .into_response())
}
