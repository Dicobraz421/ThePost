# KIT-03: In-memory шина, лизы, журнал событий, часы

lane: testkit
deps: [CTR-04]
class: A
files: [crates/tp-testkit/src/infra.rs]
read: [docs/CONTRACTS.md#EventBus, docs/CONTRACTS.md#Lease, docs/CONTRACTS.md#Events]
checks: [nextest]
manual: no

## Сделать
- MemEventBus, MemLease, MemEventLog (разделы, курсоры); ManualClock для тестов таймаутов.

## Приёмка
- Тесты: истечение лизы по ManualClock; подписчик после рестарта продолжает с сохранённой позиции; порядок в пределах ящика.
