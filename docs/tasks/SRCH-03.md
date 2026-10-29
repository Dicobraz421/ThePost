# SRCH-03: Исполнитель запросов

lane: search
deps: [SRCH-01]
class: A
files: [crates/tp-search/src/query.rs]
read: [docs/spec/S4.md#4.6, docs/CONTRACTS.md#Search]
checks: [nextest]
manual: no

## Сделать
- SearchBackend.query: разбор TextQuery, сортировка по дате, лимит; snippet.

## Приёмка
- Тесты релевантности на корпусе.
