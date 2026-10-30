# CTR-01: Идентификаторы и ошибки

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/ids.rs, crates/tp-contracts/src/error.rs, crates/tp-contracts/src/lib.rs]
read: [docs/CONTRACTS.md#Ids, docs/CONTRACTS.md#Errors, docs/spec/S3.md#3.8]
checks: [nextest]
manual: no

## Сделать
- Newtype-идентификаторы (TenantId, DomainId, AccountId, MailboxId, MessageId), Uid, UidValidity, ModSeq, BlobHash, Gen, BlobKey с tenant в пути;
- TenantId::DEFAULT — организация по умолчанию (SRS 3.8).
- Error и Result; From для io::Error → Unavailable/Internal.

## Приёмка
- Тесты: round-trip serde и hex, формат пути BlobKey.
