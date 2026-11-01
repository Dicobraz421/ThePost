# CTR-23: контракты взаимодействия: сигналы, Rpc, NodeInfo, AbuseBlocks

lane: contracts
deps: [CTR-22]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-01-interaction-db-ha.md, docs/spec/S2.md#2.8, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `EventKind`: `LogAppended { partition }`, `MembersChanged`, `Blocked { subject }`, `Resync`; системные виды — в `subscribe_system()`.
- `NodeInfo { id, roles, addr }`, `Membership::live_info()`, регистрация ролей и адреса в `heartbeat`.
- `Rpc`/`RpcHandler` (ошибки `Busy`, `Timeout`, `Unavailable`), `AbuseBlocks`.
- Удалить `InternalChannel::Valkey`.
- In-memory реализации в `tp-testkit`.

## Приёмка
- Conformance-тесты testkit для новых трейтов; CHANGELOG в CONTRACTS.md.
