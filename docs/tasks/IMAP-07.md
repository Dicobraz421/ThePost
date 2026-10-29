# IMAP-07: APPEND, COPY, MOVE, EXPUNGE

lane: imap
deps: [IMAP-04, MS-05]
class: A
files: [crates/tp-imap/src/write.rs]
read: [docs/spec/S4.md#4.2, docs/spec/S4.md#4.7]
checks: [nextest]
manual: no

## Сделать
- APPEND (MailStore.ingest), COPY, MOVE, EXPUNGE/UID EXPUNGE в корзину, UIDPLUS.

## Приёмка
- Тесты на testkit.
