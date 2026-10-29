# KIT-01: In-memory BlobStore и BlobIndex

lane: testkit
deps: [CTR-02]
class: A
files: [crates/tp-testkit/src/blob.rs]
read: [docs/CONTRACTS.md#BlobStore, docs/CONTRACTS.md#BlobIndex, docs/spec/S3.md#3.4]
checks: [nextest]
manual: no

## Сделать
- MemBlobStore (с инъекцией ошибок и задержек), MemBlobIndex с полным протоколом поколений.

## Приёмка
- Тесты протокола: acquire/commit/release, Bumped при deleting, finalize только при неизменном gen.
