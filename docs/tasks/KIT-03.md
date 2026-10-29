# KIT-03: In-memory шина, лизы, outbox, часы

lane: testkit
deps: [CTR-04]
class: A
files: [crates/tp-testkit/src/infra.rs]
read: [docs/CONTRACTS.md#EventBus, docs/CONTRACTS.md#Lease, docs/CONTRACTS.md#Outbox]
checks: [nextest]
manual: no

## Сделать
- MemEventBus, MemLease, MemOutbox; ManualClock для тестов таймаутов.

## Приёмка
- Тесты: истечение лизы по ManualClock, claim не выдаёт задание дважды.
