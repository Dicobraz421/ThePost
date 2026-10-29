# CTR-03: Метаданные ящиков и журнал изменений

lane: contracts
deps: [CTR-01]
class: B
files: [crates/tp-contracts/src/meta.rs]
read: [docs/CONTRACTS.md#Metadata, docs/CONTRACTS.md#ChangeLog, docs/spec/S3.md#3.6]
checks: [nextest]
manual: no

## Сделать
- AccountStore, MailboxStore, MessageStore, ChangeLog; NewMessage, PartRef, MessageMeta, FlagOp, Changes.

## Приёмка
- Трейты object-safe; типы Serialize/Deserialize; описаны инварианты Uid/ModSeq.
