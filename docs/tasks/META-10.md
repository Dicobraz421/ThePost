# META-10: Корзина, квоты, политики на PostgreSQL

lane: meta
deps: [CTR-07, META-04]
class: B
files: [crates/tp-meta/src/retention.rs, crates/tp-meta/migrations/*]
read: [docs/CONTRACTS.md#Trash, Retention, Quota (Ф1)]
checks: [nextest]
manual: no

## Сделать
- Таблицы корзины и политик (цепочка организация → домен → ящик → папка); выборка истёкших элементов; учёт квот.

## Приёмка
- Тесты: разрешение политики по цепочке; квота считается.
