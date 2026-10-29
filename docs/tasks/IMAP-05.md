# IMAP-05: STORE и CONDSTORE

lane: imap
deps: [IMAP-04]
class: A
files: [crates/tp-imap/src/store.rs]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- STORE/UID STORE, MODSEQ, UNCHANGEDSINCE, CHANGEDSINCE.

## Приёмка
- Тесты по RFC 7162.
