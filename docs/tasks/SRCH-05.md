# SRCH-05: Сегменты в BlobStore

lane: search
deps: [SRCH-02, META-12]
class: A
files: [crates/tp-search/src/persist.rs]
read: [docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- После коммита — загрузка новых файлов сегментов в BlobStore (put_raw), манифест с CAS; загрузка индекса по манифесту.

## Приёмка
- Тест: индекс восстанавливается на «другом узле» из BlobStore.
