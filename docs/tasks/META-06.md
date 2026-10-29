# META-06: Outbox

lane: meta
deps: [META-01, CTR-04]
class: A
files: [crates/tp-meta/src/outbox.rs]
read: [docs/CONTRACTS.md#Outbox]
checks: [nextest]
manual: no

## Сделать
- push в транзакции вызывающего, claim через FOR UPDATE SKIP LOCKED с лизой, done, retry_at.

## Приёмка
- Конкурентный тест: 4 воркера не получают одно задание.
