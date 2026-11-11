# JMAP-33: JMAP Tasks поверх VTODO

lane: jmap
deps: [JMAP-29]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

Этап 4, «рюшечки»: задачи — только транспорт синхронизации (SRS 4.4).

## Сделать
- Capability задач по текущей редакции IETF: `TaskList` и `Task` (`/get`, `/changes`, `/set`, `/query`) поверх коллекций VTODO.

## Приёмка
- Тесты: задача из JMAP видна в CalDAV (Thunderbird) и наоборот.
