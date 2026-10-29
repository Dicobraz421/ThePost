# BLOB-03: BlobWriter: протокол записи

lane: blob
deps: [CTR-02, KIT-01]
class: A
files: [crates/tp-blob/src/writer.rs]
read: [docs/spec/S3.md#3.4]
checks: [nextest]
manual: no

## Сделать
- Потоковый SHA-256, acquire → upload → commit_new с обработкой Bumped; вне транзакций БД; метрики.

## Приёмка
- Тесты на testkit: дедупликация, Bumped, ошибка загрузки не создаёт запись.
