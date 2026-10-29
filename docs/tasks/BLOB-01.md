# BLOB-01: Локальный BlobStore

lane: blob
deps: [CTR-02]
class: A
files: [crates/tp-blob/src/local.rs]
read: [docs/spec/S3.md#3.2]
checks: [nextest]
manual: no

## Сделать
- POSIX-директория: запись во временный файл + fsync + rename, раскладка по префиксу хэша, диапазонное чтение, list по mtime.

## Приёмка
- Тесты на tempdir: идемпотентность put, обрыв записи не оставляет видимый объект.
