# KIT-02: In-memory метаданные

lane: testkit
deps: [CTR-03]
class: A
files: [crates/tp-testkit/src/meta.rs]
read: [docs/CONTRACTS.md#Metadata, docs/CONTRACTS.md#ChangeLog]
checks: [nextest]
manual: no

## Сделать
- MemMetadata: AccountStore, MailboxStore, MessageStore, ChangeLog; Uid/ModSeq монотонны.

## Приёмка
- Тесты: монотонность, changes since, move/copy меняют ModSeq.
