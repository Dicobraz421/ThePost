//! Занятость повторяющихся событий: разворот повторов при запросе (META-54, решение 59).
//!
//! Правила повторения (`RRULE`, `RDATE`, `EXDATE`, переопределённые вхождения, часовые пояса) разворачивает calcard
//! из тела объекта; в индекс `dav_busy` периоды повторяющихся событий не пишутся.

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

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Date, Month};
    use tp_contracts::schedule::merge_busy;

    const LIMIT: usize = 20_000;

    fn t(year: i32, month: u8, day: u8, hour: u8, minute: u8) -> OffsetDateTime {
        Date::from_calendar_date(year, Month::try_from(month).expect("месяц"), day)
            .expect("дата")
            .with_hms(hour, minute, 0)
            .expect("время")
            .assume_utc()
    }

    fn event(lines: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:weekly\r\n\
             DTSTAMP:20260101T000000Z\r\n{lines}END:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    fn busy(
        body: &str,
        tz: Option<&str>,
        start: OffsetDateTime,
        end: OffsetDateTime,
    ) -> Vec<BusyPeriod> {
        merge_busy(expand(body, tz, &Period { start, end }, LIMIT).expect("тело разбирается"))
    }

    fn p(start: OffsetDateTime, end: OffsetDateTime, kind: BusyKind) -> BusyPeriod {
        BusyPeriod { start, end, kind }
    }

    /// Приёмка META-54: еженедельная встреча без конца видна через 3 года; исключённая дата не занята.
    #[test]
    fn weekly_forever_in_three_years_and_exdate() {
        // Понедельник 5 января 2026, 10:00–11:00 UTC, каждую неделю без конца; 12 февраля 2029 исключено.
        let body = event(
            "DTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\nRRULE:FREQ=WEEKLY\r\n\
             EXDATE:20290212T100000Z\r\n",
        );
        let got = busy(&body, None, t(2029, 2, 1, 0, 0), t(2029, 2, 20, 0, 0));
        assert_eq!(
            got,
            vec![
                p(t(2029, 2, 5, 10, 0), t(2029, 2, 5, 11, 0), BusyKind::Busy),
                p(t(2029, 2, 19, 10, 0), t(2029, 2, 19, 11, 0), BusyKind::Busy),
            ]
        );
    }

    /// Пояс события: 10:00 по Берлину — 09:00 UTC зимой и 08:00 UTC летом; период обрезается по интервалу.
    #[test]
    fn timezone_and_clip() {
        let body = event(
            "DTSTART;TZID=Europe/Berlin:20260105T100000\r\nDTEND;TZID=Europe/Berlin:20260105T110000\r\n\
             RRULE:FREQ=WEEKLY\r\n",
        );
        assert_eq!(
            busy(&body, None, t(2027, 1, 4, 0, 0), t(2027, 1, 4, 9, 30)),
            vec![p(t(2027, 1, 4, 9, 0), t(2027, 1, 4, 9, 30), BusyKind::Busy)]
        );
        assert_eq!(
            busy(&body, None, t(2027, 7, 5, 0, 0), t(2027, 7, 6, 0, 0)),
            vec![p(t(2027, 7, 5, 8, 0), t(2027, 7, 5, 9, 0), BusyKind::Busy)]
        );
    }

    /// Время без пояса — в поясе календаря.
    #[test]
    fn floating_time_in_calendar_timezone() {
        let body = event(
            "DTSTART:20260105T100000\r\nDTEND:20260105T110000\r\nRRULE:FREQ=DAILY;COUNT=2\r\n",
        );
        assert_eq!(
            busy(
                &body,
                Some("Asia/Tokyo"),
                t(2026, 1, 5, 0, 0),
                t(2026, 1, 6, 0, 0)
            ),
            vec![p(t(2026, 1, 5, 1, 0), t(2026, 1, 5, 2, 0), BusyKind::Busy)]
        );
    }

    /// `RDATE` добавляет вхождение; переопределённое вхождение (`RECURRENCE-ID`) переносится и может быть отменено
    /// или предварительным; прозрачное событие не занимает.
    #[test]
    fn rdate_overrides_and_status() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n\
             BEGIN:VEVENT\r\nUID:u\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260105T100000Z\r\n\
             DTEND:20260105T110000Z\r\nRRULE:FREQ=DAILY;COUNT=3\r\nRDATE:20260110T100000Z\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:u\r\nDTSTAMP:20260101T000000Z\r\nRECURRENCE-ID:20260106T100000Z\r\n\
             DTSTART:20260106T150000Z\r\nDTEND:20260106T160000Z\r\nSTATUS:TENTATIVE\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:u\r\nDTSTAMP:20260101T000000Z\r\nRECURRENCE-ID:20260107T100000Z\r\n\
             DTSTART:20260107T100000Z\r\nDTEND:20260107T110000Z\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n\
             END:VCALENDAR\r\n";
        assert_eq!(
            busy(body, None, t(2026, 1, 1, 0, 0), t(2026, 2, 1, 0, 0)),
            vec![
                p(t(2026, 1, 5, 10, 0), t(2026, 1, 5, 11, 0), BusyKind::Busy),
                p(
                    t(2026, 1, 6, 15, 0),
                    t(2026, 1, 6, 16, 0),
                    BusyKind::Tentative
                ),
                p(t(2026, 1, 10, 10, 0), t(2026, 1, 10, 11, 0), BusyKind::Busy),
            ]
        );
        let transparent = event(
            "DTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\nRRULE:FREQ=DAILY\r\nTRANSP:TRANSPARENT\r\n",
        );
        assert!(busy(&transparent, None, t(2026, 1, 1, 0, 0), t(2026, 2, 1, 0, 0)).is_empty());
    }

    /// Лимит вхождений считается от начала повторов; тело, которое не разбирается, занятости не даёт.
    #[test]
    fn limit_and_broken_body() {
        let body =
            event("DTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\nRRULE:FREQ=DAILY\r\n");
        let range = Period {
            start: t(2026, 1, 1, 0, 0),
            end: t(2027, 1, 1, 0, 0),
        };
        assert_eq!(
            expand(&body, None, &range, 3)
                .expect("тело разбирается")
                .len(),
            3
        );
        assert!(expand("не календарь", None, &range, LIMIT).is_none_or(|v| v.is_empty()));
    }
}
