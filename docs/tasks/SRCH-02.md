# SRCH-02: Индексатор

lane: search
deps: [SRCH-01, KIT-03]
class: A
files: [crates/tp-search/src/indexer.rs]
read: [docs/spec/S4.md#4.6, docs/CONTRACTS.md#Outbox]
checks: [nextest]
manual: no

## Сделать
- Роль indexer: claim заданий Outbox, лиза на ящик, пакетный коммит по времени/числу, удаление из индекса.

## Приёмка
- Тест: задержка индексации в пределах интервала коммита.
