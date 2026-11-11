# DAV-13: планирование внутри организации — RFC 6638

lane: dav
deps: [DAV-08, CTR-47]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `schedule-inbox` и `schedule-outbox`, `schedule-calendar-transp`, `schedule-default-calendar-URL`, `Schedule-Tag` и `If-Schedule-Tag-Match`.
- Неявное планирование: `PUT` организатора с `ATTENDEE` — iTIP `REQUEST`/`CANCEL` внутренним участникам (`ItipDelivery`), ответ участника
  (`PARTSTAT`) — `REPLY` организатору; `SCHEDULE-AGENT=CLIENT` — сервер не планирует; `SCHEDULE-STATUS` в объекте.
- Модуль `tp_dav::itip` (разбор и построение iTIP на calcard) — общий с JMAP (`sendSchedulingMessages`, JMAP-31).

## Приёмка
- Тесты на in-memory: приглашение двух участников, принятие, перенос, отмена, удаление участника; `Schedule-Tag` не меняется от `PARTSTAT`.
