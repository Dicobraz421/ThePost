# META-66: `by_ids` в PostgreSQL

lane: meta
deps: [CTR-63]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/tasks/CTR-63.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 49: admin-13, PR #487.

## Сделать
- `AccountStore::by_ids` одним запросом (`= ANY($1)`) с RLS.

## Приёмка
- Набор CTR-63 на PostgreSQL.
