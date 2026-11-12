# CTR-72: `AccountStore::by_external_id` — обязательный метод

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 77: auth-10, PR #543. у метода тело по умолчанию с `Error::Internal` — реализация может его «забыть».

## Сделать
- Тело по умолчанию удалить; реализации — `tp-meta`, `tp-testkit` (уже есть); CHANGELOG.

## Приёмка
- CI зелёный.
