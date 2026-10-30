# CTR-07: Корзина и квоты

lane: contracts
deps: [CTR-05]
class: B
files: [crates/tp-contracts/src/retention.rs]
read: [docs/CONTRACTS.md#Trash, Retention, Quota (Ф1), docs/spec/S4.md#4.7]
checks: [nextest]
manual: no

## Сделать
- TrashStore, Quota; срок корзины и квота — через PolicyResolver. Политики хранения и Legal Hold — модуль «Соответствие требованиям» (DecisionHook).

## Приёмка
- Трейты object-safe.
