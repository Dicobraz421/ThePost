# BLOB-02: S3 BlobStore

lane: blob
deps: [CTR-02]
class: A
files: [crates/tp-blob/src/s3.rs]
read: [docs/spec/S3.md#3.2]
checks: [nextest]
manual: no

## Сделать
- object_store (aws): endpoint, path-style, multipart для больших объектов, повторы с backoff, таймауты из конфигурации.

## Приёмка
- Тесты на InMemory из object_store; интеграционный — при TEST_S3_URL (SeaweedFS из compose).
