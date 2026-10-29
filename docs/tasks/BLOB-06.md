# BLOB-06: Поиск «сирот»

lane: blob
deps: [BLOB-05]
class: A
files: [crates/tp-blob/src/orphans.rs]
read: [docs/spec/S3.md#3.4]
checks: [nextest]
manual: no

## Сделать
- list(старше grace) → exists в BlobIndex → удаление; низкий приоритет, лимит скорости.

## Приёмка
- Тест: объект после обрыва commit удаляется; молодой объект не трогается.
