# JMAP-04: Email/query

lane: jmap
deps: [JMAP-03]
class: A
files: [crates/tp-jmap/src/email_query.rs]
read: [docs/spec/S4.md#4.2, docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- Email/query, queryChanges: фильтры, сортировка, collapseThreads, текст через SearchBackend.

## Приёмка
- Тесты с мок-SearchBackend.
