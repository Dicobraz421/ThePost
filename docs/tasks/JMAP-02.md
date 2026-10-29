# JMAP-02: Mailbox

lane: jmap
deps: [JMAP-01, MS-04]
class: A
files: [crates/tp-jmap/src/mailbox.rs]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- Mailbox/get, changes, query, queryChanges, set (RFC 8621).

## Приёмка
- Тесты на testkit.
