# META-10: Корзина и хранение на PostgreSQL

lane: meta
deps: [CTR-07, META-04]
class: B
files: [crates/tp-meta/src/retention.rs, crates/tp-meta/migrations/*]
read: [docs/CONTRACTS.md#Trash, Retention, Quota (Ф1)]
checks: [nextest]
manual: no

## Сделать
- Таблицы политик и Legal Hold; выборка истёкших элементов с учётом hold; учёт квот.

## Приёмка
- Тесты: hold блокирует истечение; квота считается.
