# CTR-09: IndexManifestStore и задания поиска

lane: contracts
deps: [CTR-05]
class: B
files: [crates/tp-contracts/src/search.rs]
read: [docs/CONTRACTS.md#Search, docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- Уточнение типов манифеста (версия, сегменты, размер) и заданий Outbox поиска.

## Приёмка
- Типы сериализуются (тест).
