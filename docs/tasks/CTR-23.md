# CTR-23: контракты настроек транспорта и правил потока

lane: contracts
deps: [CTR-22]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-01-transport-scope-external-mta.md, docs/spec/S4.md#4.1, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `transport::{TransportStore, ClusterTransport, TenantTransport, AcceptedDomain, DomainKind, TrustedSource, SourceMatch, SourceKind, DestinationPolicy, FlowRule, Condition, Action, RuleMode}`.
- `MailFlow`, `FlowInput`, `FlowOutcome`; `EventKind::TransportChanged { tenant: Option<TenantId> }`.
- In-memory `MemTransport` в `tp-testkit`, набор `suite::transport_store`.

## Приёмка
- Conformance-тесты: запись с устаревшей `version` — `Conflict`; снимок организации не видит данных другой; CHANGELOG в CONTRACTS.md.
