# BLOB-08: Тест гонки GC × дедупликация

lane: blob
deps: [BLOB-05]
class: A
files: [crates/tp-blob/src/../tests/race.rs]
read: [docs/spec/S3.md#3.4]
checks: [nextest]
manual: no

## Сделать
- proptest: случайные чередования ingest/release/GC/orphans; на testkit и на PostgreSQL при TEST_DATABASE_URL.

## Приёмка
- Инвариант: объект с refcnt>0 всегда читается; 0 сирот после цикла.
