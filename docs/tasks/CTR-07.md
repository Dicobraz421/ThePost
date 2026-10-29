# CTR-07: Корзина, хранение, квоты

lane: contracts
deps: [CTR-05]
class: B
files: [crates/tp-contracts/src/retention.rs]
read: [docs/CONTRACTS.md#Trash, Retention, Quota (Ф1), docs/spec/S4.md#4.7]
checks: [nextest]
manual: no

## Сделать
- TrashStore, RetentionPolicy, LegalHold, Quota.

## Приёмка
- Трейты object-safe; разрешение политики по уровням (тест).
