//! Индекс объекта ([`ObjectIndex`]) из тела iCalendar или vCard (calcard): компонент, UID, интервал с развёрнутыми
//! повторами. Им пользуются `tp-dav` (`PUT`) и `tp-jmap` (CalendarEvent, ContactCard).
//!
//! Интервал — от начала первого вхождения до конца последнего (UTC; даты без часового пояса — как UTC). Повторы
//! разворачиваются до горизонта [`IndexOptions::horizon`] и не более [`IndexOptions::max_instances`] вхождений;
//! правило без `COUNT` и `UNTIL`, вхождение позже горизонта или достигнутый лимит — интервал без конца. Ошибка дат
//! (нет `DTSTART` у события, неверное правило) или объект без дат (задача без `DTSTART` и `DUE`) — без интервала:
//! такой объект — кандидат любого запроса time-range (CONTRACTS.md#Dav).
//!
//! Занятость ([`ObjectIndex::busy`], CONTRACTS.md#Schedule) — вхождения событий до горизонта развёртки, не больше
//! `MAX_BUSY_PERIODS`: `TRANSP:TRANSPARENT`, `STATUS:CANCELLED` и вхождения нулевой длины не занимают,
//! `STATUS:TENTATIVE` — отдельным видом; исключения (`RECURRENCE-ID`, `EXDATE`) учтены. Горизонт занятости
//! хранилища (`DavLimits::busy_horizon`) применяет [`clip_busy`].

use calcard::common::timezone::Tz;
use calcard::icalendar::dates::CalendarExpand;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarProperty, ICalendarStatus,
    ICalendarTransparency, ICalendarValue,
};
use calcard::vcard::{VCard, VCardProperty, VCardValue};
use time::OffsetDateTime;
use tp_contracts::dav::{CardSearch, CollectionKind, Component, ObjectIndex, Span};
use tp_contracts::schedule::{BusyKind, BusyPeriod, MAX_BUSY_PERIODS, merge_busy};

/// Параметры развёртки повторов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexOptions {
    /// Горизонт: вхождения позже — интервал без конца.
    pub horizon: OffsetDateTime,
    /// Наибольшее число развёрнутых вхождений.
    pub max_instances: usize,
}

/// Тело не подходит для индекса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IndexError {
    /// Не разбирается как VCALENDAR или нет компонента VEVENT, VTODO, VJOURNAL.
    #[error("недопустимые данные iCalendar")]
    Calendar,
    /// Не разбирается как VCARD.
    #[error("недопустимые данные vCard")]
    Card,
    /// Нет UID или он недопустим.
    #[error("нет допустимого UID")]
    Uid,
    /// Компоненты разных видов или с разными UID в одном ресурсе (RFC 4791 §4.1).
    #[error("компоненты разных видов или UID в одном ресурсе")]
    Mixed,
}

/// Индекс тела объекта коллекции вида `kind`.
pub fn index(
    body: &str,
    kind: CollectionKind,
    opts: &IndexOptions,
) -> Result<ObjectIndex, IndexError> {
    match kind {
        CollectionKind::Calendar => calendar(body, opts),
        CollectionKind::AddressBook => card(body),
    }
}

/// Индекс vCard: UID обязателен (RFC 6352 §5.1); поисковые строки (CTR-54) — полное имя (`FN`), части имени (`N`),
/// псевдонимы (`NICKNAME`) и адреса (`EMAIL`).
pub fn card(body: &str) -> Result<ObjectIndex, IndexError> {
    let v = VCard::parse(body).map_err(|_| IndexError::Card)?;
    let uid = v.uid().ok_or(IndexError::Uid)?.to_owned();
    let searched = [
        VCardProperty::Fn,
        VCardProperty::N,
        VCardProperty::Nickname,
        VCardProperty::Email,
    ];
    let strings = v
        .entries
        .iter()
        .filter(|e| searched.contains(&e.name))
        .flat_map(|e| &e.values)
        .flat_map(|value| match value {
            VCardValue::Text(text) => std::slice::from_ref(text),
            VCardValue::Component(parts) => parts.as_slice(),
            _ => &[],
        });
    checked(ObjectIndex {
        search: CardSearch::new(strings),
        ..ObjectIndex::new(Component::Card, uid, None)
    })
}

/// Индекс iCalendar: один вид компонента и один UID на ресурс.
pub fn calendar(body: &str, opts: &IndexOptions) -> Result<ObjectIndex, IndexError> {
    let ical = ICalendar::parse(body).map_err(|_| IndexError::Calendar)?;
    of_calendar(&ical, opts)
}

/// Индекс разобранного iCalendar (см. [`calendar`]).
pub fn of_calendar(ical: &ICalendar, opts: &IndexOptions) -> Result<ObjectIndex, IndexError> {
    if ical.components.first().map(|c| &c.component_type)
        != Some(&ICalendarComponentType::VCalendar)
    {
        return Err(IndexError::Calendar);
    }
    let mut component: Option<Component> = None;
    let mut uid: Option<&str> = None;
    for comp in &ical.components {
        let c = match comp.component_type {
            ICalendarComponentType::VEvent => Component::Event,
            ICalendarComponentType::VTodo => Component::Todo,
            ICalendarComponentType::VJournal => Component::Journal,
            _ => continue,
        };
        if component.is_some_and(|x| x != c) {
            return Err(IndexError::Mixed);
        }
        component = Some(c);
        let u = comp.uid().ok_or(IndexError::Uid)?;
        if uid.is_some_and(|x| x != u) {
            return Err(IndexError::Mixed);
        }
        uid = Some(u);
    }
    let component = component.ok_or(IndexError::Calendar)?;
    let uid = uid.ok_or(IndexError::Uid)?.to_owned();
    let exp = ical.expand_dates(Tz::UTC, opts.max_instances);
    let mut idx = ObjectIndex::new(component, uid, span(ical, &exp, opts));
    if component == Component::Event {
        idx.busy = busy(ical, &exp, opts);
    }
    checked(idx)
}

/// Оставляет в индексе занятости периоды, начинающиеся раньше `until` (момент записи плюс
/// `DavLimits::busy_horizon`).
pub fn clip_busy(idx: &mut ObjectIndex, until: OffsetDateTime) {
    idx.busy.retain(|p| p.start < until);
}

/// Периоды занятости события: вхождения до горизонта, склеенные по видам.
fn busy(ical: &ICalendar, exp: &CalendarExpand, opts: &IndexOptions) -> Vec<BusyPeriod> {
    if !exp.errors.is_empty() {
        return Vec::new();
    }
    let at = |t: i64| OffsetDateTime::from_unix_timestamp(t).ok();
    let mut out = Vec::new();
    for e in &exp.events {
        let Some(comp) = ical.component_by_id(e.comp_id) else {
            continue;
        };
        let (s, en) = e.timestamps();
        if comp.component_type != ICalendarComponentType::VEvent
            || en <= s
            || comp.transparency() == Some(&ICalendarTransparency::Transparent)
            || comp.status() == Some(&ICalendarStatus::Cancelled)
        {
            continue;
        }
        let (Some(start), Some(end)) = (at(s), at(en)) else {
            continue;
        };
        if start > opts.horizon {
            continue;
        }
        let kind = if comp.status() == Some(&ICalendarStatus::Tentative) {
            BusyKind::Tentative
        } else {
            BusyKind::Busy
        };
        out.push(BusyPeriod { start, end, kind });
    }
    let mut out = merge_busy(out);
    out.truncate(MAX_BUSY_PERIODS);
    out
}

fn checked(idx: ObjectIndex) -> Result<ObjectIndex, IndexError> {
    idx.validate().map_err(|_| IndexError::Uid)?;
    Ok(idx)
}

fn span(ical: &ICalendar, exp: &CalendarExpand, opts: &IndexOptions) -> Option<Span> {
    if !exp.errors.is_empty() || exp.events.is_empty() {
        return None;
    }
    let horizon = opts.horizon.unix_timestamp();
    let mut open = exp.events.len() >= opts.max_instances || ical.components.iter().any(unbounded);
    let (mut start, mut end) = (i64::MAX, i64::MIN);
    for e in &exp.events {
        let (s, en) = e.timestamps();
        start = start.min(s);
        end = end.max(en).max(s);
        open |= s > horizon;
    }
    let start = OffsetDateTime::from_unix_timestamp(start).ok()?;
    let end = if open {
        None
    } else {
        Some(OffsetDateTime::from_unix_timestamp(end).ok()?)
    };
    Some(Span { start, end })
}

/// Правило повтора без `COUNT` и `UNTIL`.
fn unbounded(c: &ICalendarComponent) -> bool {
    c.properties(&ICalendarProperty::Rrule).any(|e| {
        matches!(e.values.first(), Some(ICalendarValue::RecurrenceRule(r)) if r.until.is_none() && r.count.is_none())
    })
}

#[cfg(test)]
mod tests {
    use time::{Date, Month};

    use super::*;

    fn at(y: i32, m: Month, d: u8, h: u8) -> OffsetDateTime {
        Date::from_calendar_date(y, m, d)
            .expect("date")
            .with_hms(h, 0, 0)
            .expect("time")
            .assume_utc()
    }

    fn opts() -> IndexOptions {
        IndexOptions {
            horizon: at(2028, Month::January, 1, 0),
            max_instances: 1_000,
        }
    }

    fn event(extra: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:ev-1\r\n\
             DTSTAMP:20260101T000000Z\r\nDTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\n\
             SUMMARY:Планёрка\r\n{extra}END:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    #[test]
    fn single_event() {
        let idx = calendar(&event(""), &opts()).expect("index");
        assert_eq!(idx.component, Component::Event);
        assert_eq!(idx.uid, "ev-1");
        assert_eq!(
            idx.span,
            Some(Span {
                start: at(2026, Month::January, 5, 10),
                end: Some(at(2026, Month::January, 5, 11)),
            })
        );
    }

    #[test]
    fn recurring_event() {
        let weekly = calendar(&event("RRULE:FREQ=WEEKLY;COUNT=3\r\n"), &opts()).expect("index");
        assert_eq!(
            weekly.span,
            Some(Span {
                start: at(2026, Month::January, 5, 10),
                end: Some(at(2026, Month::January, 19, 11)),
            })
        );
        let until = calendar(
            &event("RRULE:FREQ=DAILY;UNTIL=20260107T100000Z\r\nEXDATE:20260107T100000Z\r\n"),
            &opts(),
        )
        .expect("index");
        assert_eq!(
            until.span.and_then(|s| s.end),
            Some(at(2026, Month::January, 6, 11)),
            "исключённое последнее вхождение не входит"
        );
        for rule in [
            "RRULE:FREQ=DAILY\r\n",
            "RRULE:FREQ=YEARLY;COUNT=10\r\n",
            "RRULE:FREQ=MINUTELY;COUNT=5000\r\n",
        ] {
            let idx = calendar(&event(rule), &opts()).expect("index");
            assert_eq!(
                idx.span,
                Some(Span {
                    start: at(2026, Month::January, 5, 10),
                    end: None
                }),
                "{rule}"
            );
        }
    }

    #[test]
    fn busy_index() {
        let p = |d: u8, h1: u8, h2: u8, kind| BusyPeriod {
            start: at(2026, Month::January, d, h1),
            end: at(2026, Month::January, d, h2),
            kind,
        };
        // Повтор с исключённым вхождением и перенесённым (под вопросом) вхождением.
        let body = event("RRULE:FREQ=DAILY;COUNT=4\r\nEXDATE:20260106T100000Z\r\n").replace(
            "END:VCALENDAR",
            "BEGIN:VEVENT\r\nUID:ev-1\r\nRECURRENCE-ID:20260107T100000Z\r\nDTSTART:20260107T140000Z\r\n\
             DTEND:20260107T150000Z\r\nSTATUS:TENTATIVE\r\nEND:VEVENT\r\nEND:VCALENDAR",
        );
        let mut idx = calendar(&body, &opts()).expect("index");
        assert_eq!(
            idx.busy,
            vec![
                p(5, 10, 11, BusyKind::Busy),
                p(7, 14, 15, BusyKind::Tentative),
                p(8, 10, 11, BusyKind::Busy),
            ]
        );
        clip_busy(&mut idx, at(2026, Month::January, 7, 0));
        assert_eq!(idx.busy, vec![p(5, 10, 11, BusyKind::Busy)]);
        for free in ["TRANSP:TRANSPARENT\r\n", "STATUS:CANCELLED\r\n"] {
            assert!(
                calendar(&event(free), &opts())
                    .expect("index")
                    .busy
                    .is_empty(),
                "{free}"
            );
        }
        // Повтор без конца — до горизонта, не больше лимита контракта.
        let open = calendar(&event("RRULE:FREQ=DAILY\r\n"), &opts()).expect("index");
        assert!(!open.busy.is_empty() && open.busy.len() <= MAX_BUSY_PERIODS);
        assert!(open.busy.iter().all(|b| b.start <= opts().horizon));
        open.validate().expect("valid");
    }

    #[test]
    fn todo_and_card() {
        let todo = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:x\r\nBEGIN:VTODO\r\nUID:t-1\r\nDTSTAMP:20260101T000000Z\r\n\
                    SUMMARY:Купить\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let idx = index(todo, CollectionKind::Calendar, &opts()).expect("todo");
        assert_eq!((idx.component, idx.span), (Component::Todo, None));
        let vcard = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:c-1\r\nFN:Иван Петров\r\nN:Петров;Иван;;;\r\nEND:VCARD\r\n";
        let idx = index(vcard, CollectionKind::AddressBook, &opts()).expect("card");
        assert_eq!((idx.component, idx.uid.as_str()), (Component::Card, "c-1"));
        assert_eq!(idx.search.strings, ["иван петров", "петров", "иван"]);
        let mail = vcard.replace("END:VCARD", "EMAIL;TYPE=work:Ivan@Example.com\r\nEND:VCARD");
        let idx = card(&mail).expect("card");
        assert!(idx.search.matches("ivan@ex") && idx.search.matches("ПЕТР"));
    }

    #[test]
    fn rejects() {
        assert_eq!(calendar("not ical", &opts()), Err(IndexError::Calendar));
        assert_eq!(
            calendar(&event("").replace("UID:ev-1\r\n", ""), &opts()),
            Err(IndexError::Uid)
        );
        let two = event("").replace(
            "END:VCALENDAR",
            "BEGIN:VEVENT\r\nUID:ev-2\r\nDTSTART:20260106T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR",
        );
        assert_eq!(calendar(&two, &opts()), Err(IndexError::Mixed));
        assert_eq!(
            card("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:X\r\nEND:VCARD\r\n"),
            Err(IndexError::Uid)
        );
        assert_eq!(
            card("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"),
            Err(IndexError::Card)
        );
    }
}
