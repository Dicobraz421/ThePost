# CTR-02: BlobStore и BlobIndex

lane: contracts
deps: [CTR-01]
class: B
files: [crates/tp-contracts/src/blob.rs]
read: [docs/CONTRACTS.md#BlobStore, docs/CONTRACTS.md#BlobIndex, docs/spec/S3.md#3.4]
checks: [nextest]
manual: no

## Сделать
- Трейты BlobStore и BlobIndex, типы Acquire, Commit, ByteStream.
- Doc-комментарии с протоколом 3.4 по шагам.

## Приёмка
- Трейты object-safe (`Arc<dyn …>`), doc-тест с псевдоиспользованием протокола компилируется.
