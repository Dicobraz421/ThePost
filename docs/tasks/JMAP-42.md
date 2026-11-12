# JMAP-42: `CalendarEventNotification/query` и изменения по журналу входящих

lane: jmap
deps: [CTR-65, JMAP-31]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/tasks/CTR-65.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 55: jmap-19a, PR #493.

## Сделать
- `CalendarEventNotification/query` (фильтры по времени и событию) и `/changes` по `inbox_changes` (CTR-65).

## Приёмка
- Тесты: новое приглашение видно в `/changes` и `/query`.
