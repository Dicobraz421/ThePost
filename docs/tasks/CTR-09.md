# CTR-09: IndexManifestStore и перестройка индекса

lane: contracts
deps: [CTR-05]
class: B
files: [crates/tp-contracts/src/search.rs]
read: [docs/CONTRACTS.md#Search, docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- Уточнение типов манифеста (версия, сегменты, размер) и операции перестройки индекса.

## Приёмка
- Типы сериализуются (тест).
