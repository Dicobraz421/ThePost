# CTR-30: хранилище Identity и EmailSubmission

lane: contracts
deps: [CTR-28]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md, crates/tp-testkit/src/*]
read: []
checks: [nextest]
manual: no

## Сделать
- `IdentityStore` (адреса отправителя учётной записи, подпись) и учёт отправок (`EmailSubmission`: статус, отмена до отправки) — трейты, in-memory реализация и общий набор (ветка `x/contracts-18`). Реализация в tp-meta — карточка meta.

## Приёмка
- Наборы `suite::identity_store`, `suite::submission_store` проходят на in-memory.
