# META-44: очередь модерации без группы в PostgreSQL

lane: meta
deps: [CTR-43]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Решение владельца 2026-11-11, п. 9.

## Сделать
- Миграция: `group` очереди модерации допускает NULL, колонки модераторов и источника; реализация CTR-43 в `PgMeta`.

## Приёмка
- Общий набор CTR-43 на PostgreSQL.
