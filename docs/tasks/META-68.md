# META-68: `by_address` в PostgreSQL

lane: meta
deps: [CTR-62]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/tasks/CTR-62.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 46: dav-04, PR #485.

## Сделать
- `by_address` одним запросом по учётным записям, псевдонимам и группам с RLS.

## Приёмка
- Набор CTR-62 на PostgreSQL.
