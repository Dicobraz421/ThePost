# META-02: BlobIndex на PostgreSQL

lane: meta
deps: [META-01, CTR-02]
class: A
files: [crates/tp-meta/src/blob_index.rs]
read: [docs/spec/S3.md#3.4, docs/CONTRACTS.md#BlobIndex]
checks: [nextest]
manual: no

## Сделать
- Протокол поколений: acquire (UPDATE … WHERE state='live'), commit_new (INSERT … ON CONFLICT с Bumped),
- release (zero_since), gc_candidates, mark_deleting, finalize_delete с проверкой gen.

## Приёмка
- Конкурентный тест: 32 задачи acquire/release/GC на одном хэше без потери ссылок.
