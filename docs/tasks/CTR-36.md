# CTR-36: точка расширения источников занятости, публикации календарей

lane: contracts
deps: [CTR-35]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-04-calendar-external-api.md, docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `AvailabilitySource { name, lookup(&[Address], Span) -> Vec<(Address, AvailabilityResult::{Busy(Vec<BusyPeriod>, Detail), NoData})> }`, `Registry::add_availability_source` (модуль «Федерация календарей»).
- `CalendarPublications`: `create(scope, calendar, detail, expires) -> Link`, `list`, `revoke`; уровень детализации — не выше политики.
- In-memory реализации в `tp-testkit`.

## Приёмка
- Conformance-тесты; CHANGELOG в CONTRACTS.md.
