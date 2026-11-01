# META-32: чтение с реплик для фоновых задач

lane: meta
deps: [META-30]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/spec/S3.md#3.1, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `ReadIntent::{Primary, Stale}`, `PgMeta::with_intent`; адрес реплики для чтения — у шарда (`read_replica`), для основной БД — `database.read_replica`.
- `Stale`: отставание по `pg_last_xact_replay_timestamp()` ≤ `database.replica_max_lag` (30 с), иначе и при недоступности — primary; конфликт с восстановлением (40001) — повтор на primary.

## Приёмка
- Тесты: при отставании выше порога и без реплики чтение идёт на primary; протокольные хранилища `Stale` не используют.
