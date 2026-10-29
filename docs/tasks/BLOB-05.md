# BLOB-05: GC-воркер

lane: blob
deps: [BLOB-03, KIT-03]
class: A
files: [crates/tp-blob/src/gc.rs]
read: [docs/spec/S3.md#3.4, docs/spec/S3.md#3.5]
checks: [nextest]
manual: no

## Сделать
- Цикл: gc_candidates(старше W) → mark_deleting → delete объекта → finalize_delete; пакеты, лимит скорости, лиза лидера.

## Приёмка
- Тест: объект с ссылкой не удаляется; удаление после W; метрики.
