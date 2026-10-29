# CTR-04: EventBus, Lease, Outbox, роли

lane: contracts
deps: [CTR-01]
class: B
files: [crates/tp-contracts/src/bus.rs, crates/tp-contracts/src/lease.rs, crates/tp-contracts/src/outbox.rs, crates/tp-contracts/src/role.rs]
read: [docs/CONTRACTS.md#EventBus, docs/CONTRACTS.md#Lease, docs/CONTRACTS.md#Outbox, docs/CONTRACTS.md#Roles, docs/CONTRACTS.md#Config]
checks: [nextest]
manual: no

## Сделать
- Трейты EventBus, LeaseStore, Outbox; Event, Role, RoleService, RoleCtx, ReadyHandle.

## Приёмка
- Трейты object-safe; Role ↔ строка конфигурации (FromStr/Display) с тестом.
