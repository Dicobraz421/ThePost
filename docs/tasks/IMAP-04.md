# IMAP-04: FETCH

lane: imap
deps: [IMAP-03, MS-03, MS-07]
class: A
files: [crates/tp-imap/src/fetch.rs]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- FLAGS, UID, INTERNALDATE, RFC822.SIZE, ENVELOPE, BODYSTRUCTURE, BODY[]/BODY.PEEK, BINARY, partial.

## Приёмка
- Снимки ответов на корпусе.
