# META-74: `hidden_message` в PostgreSQL

lane: meta
deps: [CTR-75]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/tasks/CTR-75.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 84: mailstore-10, PR #528.

## Сделать
- Реализация CTR-75 в `PgMeta` (шард ящика, RLS).

## Приёмка
- Набор CTR-75 на PostgreSQL.
