# BLOB-04: NVMe-кэш объектов

lane: blob
deps: [BLOB-01]
class: A
files: [crates/tp-blob/src/cache.rs]
read: [docs/spec/S3.md#3.2]
checks: [nextest]
manual: no

## Сделать
- Read-through кэш поверх BlobStore на локальном каталоге, LRU по суммарному размеру, индекс в памяти (moka).

## Приёмка
- Тесты: вытеснение по размеру, повторное чтение без обращения к источнику.
