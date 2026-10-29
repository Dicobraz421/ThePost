# IMAP-06: SEARCH

lane: imap
deps: [IMAP-04]
class: A
files: [crates/tp-imap/src/search.rs]
read: [docs/spec/S4.md#4.2, docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- SEARCH/UID SEARCH, ESEARCH: критерии по метаданным, TEXT/BODY через SearchBackend.

## Приёмка
- Тесты на testkit с мок-SearchBackend.
