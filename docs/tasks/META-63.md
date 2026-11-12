# META-63: книга GAL в PostgreSQL

lane: meta
deps: [CTR-60]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md#Dav]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 72.

## Сделать
- Реализация CTR-60 в `PgDav`: коллекция GAL организации (миграция, если нужен признак коллекции), `sync_gal` — одной
  транзакцией с `with_tenant`, сравнение по хэшу содержимого (неизменные строки не трогаются), надгробия в журнале.

## Приёмка
- Общий набор тестов CTR-60 на PostgreSQL (при `TEST_DATABASE_URL`).
