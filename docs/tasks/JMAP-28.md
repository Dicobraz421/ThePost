# JMAP-28: JMAP Calendars — CalendarEvent

lane: jmap
deps: [JMAP-27]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: docs/manual-tests/JMAP-28.md

## Сделать
- `CalendarEvent/get`, `/changes`, `/set`, `/copy`; JSCalendar ↔ iCalendar — calcard; хранится iCalendar (общий объект с CalDAV),
  индекс — `tp_dav::index`; `recurrenceOverrides`, часовые пояса, `useDefaultAlerts`.
- Ручная проверка клиентом Postman: календарь и контакты по JMAP, правка в Postman видна в Thunderbird по CalDAV и обратно.

## Приёмка
- Тесты: событие из JMAP читается CalDAV и наоборот (повтор с исключением, весь день, участники); сценарии ручной проверки без замечаний.
