# JMAP-30: JMAP — Principal и занятость

lane: jmap
deps: [JMAP-29, CTR-47]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `Principal/get`, `/query` (пользователи и ресурсы организации), `Principal/getAvailability` поверх `FreeBusy`; jmap-09
  (`Availability/get`, этап 3) использует тот же источник.

## Приёмка
- Тесты на in-memory: занятость двух пользователей, ресурс в `Principal/query`.
