# SRCH-01: Схема индекса и менеджер

lane: search
deps: [CTR-09]
class: A
files: [crates/tp-search/src/index.rs]
read: [docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- Схема Tantivy (тема, адреса, тело, вложения, дата — fast field), открытие/создание индекса ящика в каталоге.

## Приёмка
- Тест индексации и поиска 1 000 писем.
