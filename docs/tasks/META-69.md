# META-69: журнал входящих планирования в PostgreSQL

lane: meta
deps: [CTR-65, META-51]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*]
read: [docs/tasks/CTR-65.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 55: jmap-19a, PR #493.

## Сделать
- `inbox_state`, `inbox_changes`, `received_at` по таблице `dav_inbox` (META-51); надгробия входящих.

## Приёмка
- Набор CTR-65 на PostgreSQL.
