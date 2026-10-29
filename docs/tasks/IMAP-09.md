# IMAP-09: QRESYNC

lane: imap
deps: [IMAP-05]
class: A
files: [crates/tp-imap/src/qresync.rs]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- SELECT QRESYNC, VANISHED по ChangeLog.

## Приёмка
- Тесты по RFC 7162.
