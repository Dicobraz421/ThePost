# JMAP-29: JMAP Calendars — CalendarEvent/query и развёртывание повторов

lane: jmap
deps: [JMAP-28]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

## Сделать
- `CalendarEvent/query` (`inCalendars`, `after`, `before`, `text`, `title`, `uid`; сортировка `start`), `expandRecurrences` по
  `tp_dav::index`, `/queryChanges`.

## Приёмка
- Тесты на наборе событий DAV-06 (те же ожидания, что у `calendar-query`).
