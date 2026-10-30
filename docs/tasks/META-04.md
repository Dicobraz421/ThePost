# META-04: MessageStore и ChangeLog

lane: meta
deps: [META-03]
class: A
files: [crates/tp-meta/src/messages.rs, crates/tp-meta/src/changelog.rs]
read: [docs/CONTRACTS.md#Metadata, docs/CONTRACTS.md#ChangeLog]
checks: [nextest]
manual: no

## Сделать
- insert с выдачей Uid и ModSeq (счётчики в строке ящика), флаги, copy, move, to_trash, restore;
- запись в changelog и событие ядра в журнал событий (SRS 2.5) в той же транзакции.

## Приёмка
- Тесты: монотонность Uid/ModSeq при конкурентной вставке; changes since корректны.
