# IMAP-03: Операции с ящиками

lane: imap
deps: [IMAP-02, MS-04]
class: A
files: [crates/tp-imap/src/mailbox.rs]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- LIST (EXTENDED, SPECIAL-USE, STATUS), SELECT/EXAMINE, STATUS, CREATE, DELETE, RENAME, SUBSCRIBE.

## Приёмка
- Тесты на testkit.
