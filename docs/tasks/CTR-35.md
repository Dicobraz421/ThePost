# CTR-35: контракты: соответствие Exchange

lane: contracts
deps: [CTR-34]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-04-exchange-parity.md, docs/spec/S4.md#4.1, docs/spec/S4.md#4.5, docs/spec/S5.md#5.2, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `MailboxAudit::record(AuditEntry { scope, actor, actor_kind: Owner|Delegate|Admin, action, object })`, политика записи действий владельца.
- `SenderLists` ящика: надёжные и заблокированные адреса и домены, `verdict(scope, from) -> Option<Safe|Blocked>`.
- `Directory`: почтовые контакты, динамические группы (фильтр по атрибутам), оргатрибуты (подразделение, должность, руководитель, старшинство), дерево подразделений.
- `QueueStore`: срок выпуска `release_at`, `cancel(submission)`; журнал доставки — выборка по идентификатору отправки.
- `AccountState::{Active, Disabled { until }}`, восстановление и подключение к другой учётной записи.
- In-memory реализации в `tp-testkit`.

## Приёмка
- Conformance-тесты testkit; CHANGELOG в CONTRACTS.md.
