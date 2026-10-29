# CTR-06: Очередь SMTP

lane: contracts
deps: [CTR-05]
class: B
files: [crates/tp-contracts/src/queue.rs]
read: [docs/CONTRACTS.md#Queue (Ф1), docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- QueueStore, OutMessage, RcptStatus.

## Приёмка
- Трейт object-safe.
