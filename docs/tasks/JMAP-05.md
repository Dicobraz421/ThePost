# JMAP-05: Email/set, import, copy

lane: jmap
deps: [JMAP-03, MS-05]
class: A
files: [crates/tp-jmap/src/email_set.rs]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- Email/set (keywords, mailboxIds, создание черновиков), Email/import, Email/copy.

## Приёмка
- Тесты на testkit.
