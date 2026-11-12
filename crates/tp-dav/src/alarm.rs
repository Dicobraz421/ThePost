//! Оповещения по умолчанию календаря (CTR-56): свойства CalDAV `default-alarm-vevent-datetime` и
//! `default-alarm-vevent-date` (компоненты VALARM; draft-daboo-valarm-extensions, Apple CalendarServer) ↔ поля
//! `Collection::alerts_with_time`, `alerts_without_time` — текст JSON-объекта `Id[Alert]` JSCalendar (как JMAP
//! `defaultAlertsWithTime`, `defaultAlertsWithoutTime`). Перевод — calcard через событие-обёртку: VALARM в VEVENT ↔
//! `alerts` события JSCalendar.

use std::panic::{AssertUnwindSafe, catch_unwind};

use calcard::icalendar::ICalendar;
use calcard::jscalendar::JSCalendar;

const BEGIN: &str = "BEGIN:VALARM";
const END: &str = "END:VALARM";

/// Компоненты VALARM (текст iCalendar, строки через CRLF) из оповещений `json`; не разбирается или оповещений
/// нет — `None`.
pub(crate) fn to_valarms(json: &str) -> Option<String> {
    let text = format!(
        r#"{{"@type":"Group","entries":[{{"@type":"Event","uid":"default-alarms","start":"2000-01-01T00:00:00","duration":"PT1H","alerts":{json}}}]}}"#
    );
    let ical = guarded(|| {
        JSCalendar::<String, String>::parse(&text)
            .ok()?
            .into_icalendar()
    })?;
    let ics = ical.to_string();
    valarms(&ics).map(|v| format!("{v}\r\n"))
}

/// Оповещения (текст JSON-объекта `Id[Alert]`) из компонентов VALARM `text` — как их присылает клиент (допускается
/// и VCALENDAR с ними); нет VALARM или не разбирается — `None`.
pub(crate) fn from_valarms(text: &str) -> Option<String> {
    // Строки VALARM — через CRLF, как бы их ни разделил клиент в тексте XML.
    let alarms = valarms(text)?.replace("\r\n", "\n").replace('\n', "\r\n");
    let ics = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:{}\r\nBEGIN:VEVENT\r\nUID:default-alarms\r\n\
         DTSTAMP:20000101T000000Z\r\nDTSTART:20000101T000000Z\r\nDURATION:PT1H\r\n{alarms}\r\n\
         END:VEVENT\r\nEND:VCALENDAR\r\n",
        crate::props::PRODID
    );
    guarded(|| {
        let js = ICalendar::parse(&ics)
            .ok()?
            .into_jscalendar::<String, String>();
        let event =
            js.0.iter_object()?
                .find(|(k, _)| **k == "entries")?
                .1
                .iter_array()?
                .next()?;
        let alerts = event.iter_object()?.find(|(k, _)| **k == "alerts")?.1;
        Some(alerts.to_string())
    })
}

/// Текст от первого `BEGIN:VALARM` до последнего `END:VALARM` включительно.
fn valarms(text: &str) -> Option<&str> {
    let start = text.find(BEGIN)?;
    let end = text.rfind(END)? + END.len();
    text.get(start..end)
}

/// Вызов calcard с перехватом паники: конвертер проверяет вход `debug_assert`.
fn guarded<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    catch_unwind(AssertUnwindSafe(f)).ok().flatten()
}
