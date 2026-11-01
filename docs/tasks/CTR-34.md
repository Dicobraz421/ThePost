# CTR-34: раздел журнала событий по шардам

lane: contracts
deps: [CTR-23]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/spec/S3.md#3.6, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `Partition { shard, index }`; `partition_of(account, N)` даёт `index`, шард — по справочнику; `PartitionAssigner` назначает пары (шард, раздел).
- In-memory журнал testkit с несколькими шардами.

## Приёмка
- Conformance: каждый раздел каждого шарда назначен ровно одному узлу; один шард — прежнее поведение; CHANGELOG.
