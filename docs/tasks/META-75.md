# META-75: архив и отключённые ящики в PostgreSQL

lane: meta
deps: [CTR-76, CTR-77, CTR-78]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*]
read: [docs/tasks/CTR-76.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 85, 86, 88: mailstore-10, PR #528.

## Сделать
- Поля и выборка архива (CTR-76), публикация `DisabledMailboxChanged` (CTR-77), отметка «удаляется» (CTR-78).

## Приёмка
- Наборы CTR-76…78 на PostgreSQL.
