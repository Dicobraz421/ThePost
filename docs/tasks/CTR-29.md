# CTR-29: move_to сохраняет MessageId

lane: contracts
deps: [CTR-05]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md, crates/tp-meta/src/*, crates/tp-testkit/src/*, crates/tp-jmap/src/*, crates/tp-imap/src/*]
read: [docs/CONTRACTS.md#MailStore]
checks: [nextest]
manual: no

## Сделать
- `MessageStore::move_to` сохраняет `MessageId` (меняются папка и UID), как `to_trash`/`restore`: в JMAP id письма неизменен (RFC 8621), сейчас перенос создаёт новый (PR #164). `copy` — по-прежнему новый id. Реализации и вызовы — в том же PR (ветка `x/contracts-17`).

## Приёмка
- Тест контракта (in-memory и PostgreSQL): id после `move_to` прежний; JMAP `Email/set` mailboxIds не меняет id.
