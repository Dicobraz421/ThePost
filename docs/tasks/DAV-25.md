# DAV-25: CalDAV и CardDAV — оповещения по умолчанию и коллекция по умолчанию

lane: dav
deps: [CTR-56, DAV-05]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md#Dav]
checks: [nextest]
manual: no

Хвост волны jmap-17, PR #444.

Требует подтверждения владельца: показывать в CalDAV поля CTR-56 (вариант Б агента)? (PR #444, решение 1)

## Сделать
- Свойства календаря `default-alarm-vevent-datetime`, `default-alarm-vevent-date` (чтение и `PROPPATCH`) — из
  оповещений по умолчанию `Collection` (CTR-56).
- `schedule-default-calendar-URL` (если dav-04 влита) и выбор книги и календаря по умолчанию — по признаку из
  контракта, а не по сегменту `default`.

## Приёмка
- Тесты и снимки `PROPFIND`/`PROPPATCH`: оповещение, заданное по JMAP, видно в CalDAV и наоборот.
