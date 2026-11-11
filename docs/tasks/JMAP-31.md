# JMAP-31: JMAP — приглашения на встречи

lane: jmap
deps: [JMAP-30, DAV-13]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `CalendarEvent/set` с `sendSchedulingMessages`: iTIP через `tp_dav::itip` (внутренние — `ItipDelivery`, внешние — `ItipSender`);
  `ParticipantIdentity/get` (адреса учётной записи); ответ участника — изменение своего `participationStatus`.
- `CalendarEventNotification` — по входящим планирования.

## Приёмка
- Тесты на in-memory: приглашение из JMAP приходит участнику в CalDAV, ответ возвращается организатору.
