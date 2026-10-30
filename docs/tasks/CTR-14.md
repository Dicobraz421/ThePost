# CTR-14: членство узлов и назначение разделов журнала

lane: contracts
deps: [CTR-10]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Разделы журнала событий между экземплярами (#45, вариант 3), docs/CONTRACTS.md#Events, docs/CONTRACTS.md#Lease]
checks: [nextest]
manual: no

## Сделать
- `Membership`: `heartbeat(node, ttl)`, `leave(node)`, `live() -> Vec<NodeId>` — живые узлы по записи с продлением (реализация — META-16).
- `PartitionAssigner`: `assigned(subscriber, node, partitions) -> Vec<u32>` и уведомление о смене состава
  (`watch` после CTR-11); реализация — tp-cluster (CLU-06).
- CONTRACTS.md#Events: раздел подписчика берёт только узел, которому он назначен; лиза раздела остаётся гарантией единственности.

## Приёмка
- Тест контракта на in-memory реализации из tp-testkit: смена состава узлов меняет назначение.
