# META-49: хранение скриптов Sieve и автоответа

lane: meta
deps: [CTR-46]
class: B
files: [crates/tp-meta/migrations/*, crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/spec/S3.md#3.1, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Миграция: `sieve_scripts` (учётная запись, имя, текст, активен — частичный уникальный индекс «один активный»), скрипт организации,
  `vacation` (настройки автоответа учётной записи); RLS.
- `impl SieveStore for PgMeta`, лимиты числа и размера скриптов — из секции `meta`.

## Приёмка
- Набор `suite::sieve` (CTR-46) на PostgreSQL при `TEST_DATABASE_URL`.
