# DAV-05: CalDAV — дом календарей и свойства

lane: dav
deps: [DAV-04]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/spec/S4.md#4.8]
checks: [nextest]
manual: no

## Сделать
- `calendar-home-set`, `calendar-user-address-set` (адреса учётной записи), `schedule-inbox-URL`/`schedule-outbox-URL` — заготовки для DAV-13.
- Свойства календаря: `supported-calendar-component-set`, `supported-calendar-data`, `max-resource-size`, `calendar-timezone`,
  `calendar-description`, `calendar-color` (Apple), `calendar-order`.
- Календарь по умолчанию «Календарь» и список задач «Задачи» (VTODO) создаются при первом обращении к дому, если коллекций нет.

## Приёмка
- Снимки ответов `PROPFIND` дома и календаря для Thunderbird, iOS, DAVx5; повторное обращение не создаёт второй календарь.
