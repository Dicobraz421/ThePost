# DAV-07: CalDAV — calendar-multiget и задачи VTODO

lane: dav
deps: [DAV-05]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

## Сделать
- `REPORT calendar-multiget` (RFC 4791 §7.9): `href` объектов, отсутствующие — 404 в ответе; лимит числа `href`.
- Задачи: VTODO в своей коллекции (`supported-calendar-component-set`), `time-range` для VTODO по RFC 4791 §9.9; логика задач — у клиента
  (SRS 4.4: только транспорт синхронизации).

## Приёмка
- Тесты multiget (смешанные существующие и отсутствующие); синхронизация задач Thunderbird (снимки запросов).
