# META-21: группы рассылки в PostgreSQL

lane: meta
deps: [CTR-21, META-11]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S4.md#4.5]
checks: [nextest]
manual: no

## Сделать
- Миграция: группы, участники (в т.ч. вложенные группы), ограничения отправителей, модераторы, признак скрытия, очередь модерации; ключи с tenant_id, RLS.
- `GroupStore` на PostgreSQL; раскрытие с защитой от циклов одним запросом (рекурсивный CTE).

## Приёмка
- Тесты при `TEST_DATABASE_URL`: раскрытие вложенных групп, цикл, изоляция tenant.
