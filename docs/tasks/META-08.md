# META-08: Справочник шардов

lane: meta
deps: [META-01]
class: A
files: [crates/tp-meta/src/shards.rs]
read: [docs/spec/S3.md#3.6]
checks: [nextest]
manual: no

## Сделать
- Таблица shards, выбор наименее загруженного шарда для новой учётки, разрешение account → пул.

## Приёмка
- Тест: размещение по нагрузке, разрешение известного ящика.
