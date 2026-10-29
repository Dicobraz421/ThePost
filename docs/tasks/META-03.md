# META-03: AccountStore и MailboxStore

lane: meta
deps: [META-01]
class: A
files: [crates/tp-meta/src/accounts.rs, crates/tp-meta/src/mailboxes.rs]
read: [docs/CONTRACTS.md#Metadata]
checks: [nextest]
manual: no

## Сделать
- Реализация трейтов; UIDVALIDITY при создании; специальные папки (Inbox, Sent, Drafts, Trash, Junk) при создании учётки.

## Приёмка
- Тесты CRUD и status на PostgreSQL.
