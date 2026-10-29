# JMAP-03: Email/get и Thread

lane: jmap
deps: [JMAP-02, MS-03, MS-07]
class: A
files: [crates/tp-jmap/src/email_get.rs]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- Email/get (свойства, bodyStructure, bodyValues), Email/changes, Thread/get, Thread/changes.

## Приёмка
- Снимки на корпусе.
