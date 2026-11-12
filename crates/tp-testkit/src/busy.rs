//! Занятость повторяющихся событий: разворот повторов при запросе (META-54, решение 59).
//!
//! Правила повторения (`RRULE`, `RDATE`, `EXDATE`, переопределённые вхождения, часовые пояса) разворачивает calcard
//! из тела объекта — так же, как `PgMeta` (`tp_meta::busy`); совпадение проверяет набор `suite::freebusy::recurring`.

use std::str::FromStr;

use calcard::common::timezone::Tz;
use calcard::icalendar::{
    ICalendar, ICalendarComponentType, ICalendarStatus, ICalendarTransparency,
};
use time::OffsetDateTime;
use tp_contracts::schedule::{BusyKind, BusyPeriod, Period};

/// Периоды занятости повторяющегося события `body` внутри `range`, обрезанные по нему. Время без пояса — в поясе
/// календаря `timezone` (иначе UTC). `TRANSP:TRANSPARENT` и `STATUS:CANCELLED` не занимают, `STATUS:TENTATIVE` —
/// [`BusyKind::Tentative`]. От начала повторов разворачивается не больше `limit` вхождений. Тело, которое не
/// разбирается, — `None`.
pub(crate) fn expand(
    body: &str,
    timezone: Option<&str>,
    range: &Period,
    limit: usize,
) -> Option<Vec<BusyPeriod>> {
    let ical = ICalendar::parse(body).ok()?;
    let tz = timezone
        .and_then(|t| Tz::from_str(t).ok())
        .unwrap_or(Tz::UTC);
    let mut out = Vec::new();
    for e in ical.expand_dates(tz, limit).events {
        let Some(comp) = ical.components.get(e.comp_id as usize) else {
            continue;
        };
        if comp.component_type != ICalendarComponentType::VEvent
            || comp.transparency() == Some(&ICalendarTransparency::Transparent)
            || comp.status() == Some(&ICalendarStatus::Cancelled)
        {
            continue;
        }
        let (start, end) = e.timestamps();
        let (Ok(start), Ok(end)) = (
            OffsetDateTime::from_unix_timestamp(start),
            OffsetDateTime::from_unix_timestamp(end),
        ) else {
            continue;
        };
        let kind = if comp.status() == Some(&ICalendarStatus::Tentative) {
            BusyKind::Tentative
        } else {
            BusyKind::Busy
        };
        if let Some(p) = (BusyPeriod { start, end, kind }).clip(range) {
            out.push(p);
        }
    }
    Some(out)
}
