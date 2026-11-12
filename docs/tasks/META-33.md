# META-33: хранение: списки, контакты, оргструктура, аудит, отключённые ящики

lane: meta
deps: [CTR-35, META-30]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*]
read: [docs/decisions/2026-11-04-exchange-parity.md, docs/spec/S4.md#4.7, docs/spec/S5.md#5.2, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Реализации `SenderLists`, `DynamicGroupStore`, `ContactStore` для `PgMeta` (сейчас — только в `tp-testkit`; хвост
  реестра, п. 72, PR #548).
- Таблицы с RLS: списки отправителей, контакты, динамические группы, оргатрибуты и дерево, аудит ящиков (только добавление), состояние учётной записи и срок хранения отключённого ящика.
- Очередь: `release_at` и отмена отправки.

## Приёмка
- Наборы conformance testkit проходят на PostgreSQL (при `TEST_DATABASE_URL`).
