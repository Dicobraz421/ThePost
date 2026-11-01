# CTR-31: модерация групп рассылки — доставка одобренного письма

lane: contracts
deps: [CTR-21]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md, crates/tp-testkit/src/*, crates/tp-admin/src/*]
read: []
checks: [nextest]
manual: no

## Сделать
- Трейт `Moderation` (одобрить → доставка письма группе, отказать → DSN отправителю) в tp-contracts вместо временного в tp-admin (ADM-07, PR #174); in-memory в tp-testkit (ветка `x/contracts-19`). Реализация — tp-smtp (карточка smtp).

## Приёмка
- Набор `suite::moderation` проходит на in-memory; tp-admin — на контракте.
