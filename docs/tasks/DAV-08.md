# DAV-08: CalDAV — проверка объектов и ручная проверка клиентами

lane: dav
deps: [DAV-06, DAV-07]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/spec/S4.md#4.8]
checks: [nextest]
manual: docs/manual-tests/DAV-08.md

## Сделать
- Предусловия `PUT` (RFC 4791 §5.3.2): `valid-calendar-data` (разбор calcard), `valid-calendar-object-resource` (один UID, без `METHOD`),
  `supported-calendar-component`, `no-uid-conflict` (с `href` существующего), `max-resource-size`, `min/max-date-time`.
- Ручная проверка календарей на стенде (после APP-25): Thunderbird, Календарь iOS и macOS, DAVx5 (Android), Evolution — создание,
  правка, повтор, удаление, офлайн-правка и синхронизация, два устройства одновременно.

## Приёмка
- Тесты каждого предусловия; сценарии ручной проверки без замечаний.
