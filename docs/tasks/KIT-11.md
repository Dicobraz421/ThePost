# KIT-11: `MemShardedBlobIndex` и общий набор GC по шардам

lane: testkit
deps: [BLOB-15]
class: A
files: [crates/tp-testkit/*]
read: [docs/tasks/BLOB-15.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 83: blob-08, PR #527.

## Сделать
- Учёт BLOB в памяти на нескольких шардах и набор `suite::blob_gc_shards` (тот же, что проходит `PgMeta`).

## Приёмка
- Набор проходит на реализации в памяти; META-73 подключает его на PostgreSQL.
