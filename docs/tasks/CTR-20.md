# CTR-20: MessageStore::insert_batch — все доставки одной транзакцией

lane: contracts
deps: [CTR-16]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md, crates/tp-meta/src/*, crates/tp-testkit/src/*]
read: [docs/decisions/2026-10-31-atomic-delivery.md]
checks: [nextest]
manual: no

## Сделать
- `insert_batch(tenant, Vec<(AccountScope, MailboxId, NewMessage)>) -> Vec<Inserted>`: одна транзакция (`with_tenant`), все или ни одного; события доставки — в ней же.
- Реализации в tp-meta и tp-testkit (`MemMetadata`) — в том же PR (ветка `x/contracts-09`).

## Приёмка
- Тест: сбой на последней вставке → ни одного письма и события ни в одной папке (in-memory и PostgreSQL).
