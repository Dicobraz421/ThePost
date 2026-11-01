# META-24: признак сжатия в учёте BLOB

lane: meta
deps: [CTR-21, META-02]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S3.md#3.2]
checks: [nextest]
manual: no

## Сделать
- Колонка сжатия в учёте BLOB (по умолчанию none), `BlobIndex` возвращает признак.

## Приёмка
- Тест миграции на существующих данных.
