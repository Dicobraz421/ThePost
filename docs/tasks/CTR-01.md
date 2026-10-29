# CTR-01: Идентификаторы и ошибки

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/ids.rs, crates/tp-contracts/src/error.rs, crates/tp-contracts/src/lib.rs]
read: [docs/CONTRACTS.md#Ids, docs/CONTRACTS.md#Errors]
checks: [nextest]
manual: no

## Сделать
- Newtype-идентификаторы, Uid, UidValidity, ModSeq, BlobHash (hex Display/FromStr), Gen, BlobKey с путём объекта.
- Error и Result; From для io::Error → Unavailable/Internal.

## Приёмка
- Тесты: round-trip serde и hex, формат пути BlobKey.
