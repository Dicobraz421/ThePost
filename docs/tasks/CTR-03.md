# CTR-03: Метаданные ящиков и журнал изменений

lane: contracts
deps: [CTR-01]
class: B
files: [crates/tp-contracts/src/meta.rs]
read: [docs/CONTRACTS.md#Metadata, docs/CONTRACTS.md#ChangeLog, docs/CONTRACTS.md#Tenant, docs/spec/S3.md#3.6, docs/spec/S3.md#3.8]
checks: [nextest]
manual: no

## Сделать
- AccountStore, MailboxStore, MessageStore, ChangeLog — все операции в контексте TenantId; NewMessage, PartRef, MessageMeta, FlagOp, Changes;
- PolicyResolver: разрешение политик по цепочке организация → домен → ящик (→ папка).

## Приёмка
- Трейты object-safe; типы Serialize/Deserialize; описаны инварианты Uid/ModSeq.
