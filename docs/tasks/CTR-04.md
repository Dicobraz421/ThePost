# CTR-04: EventBus, Lease, журнал событий, роли и пресеты

lane: contracts
deps: [CTR-01]
class: B
files: [crates/tp-contracts/src/bus.rs, crates/tp-contracts/src/lease.rs, crates/tp-contracts/src/events.rs, crates/tp-contracts/src/role.rs]
read: [docs/CONTRACTS.md#EventBus, docs/CONTRACTS.md#Lease, docs/CONTRACTS.md#Events, docs/CONTRACTS.md#Roles, docs/CONTRACTS.md#Config, docs/spec/S2.md#2.1, docs/spec/S2.md#2.5]
checks: [nextest]
manual: no

## Сделать
- Трейты EventBus, LeaseStore, EventLog (журнал событий ядра с разделами и курсорами подписчиков); CoreEvent;
- Role (включая Submission), RolePreset (all, edge, transport, access, backend, mailbox) с раскрытием в набор ролей; RoleService, RoleCtx, ReadyHandle.

## Приёмка
- Трейты object-safe; Role и RolePreset ↔ строка конфигурации; раскрытие пресетов по таблице SRS 2.1 (тест).
