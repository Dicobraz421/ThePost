# CTR-26: очистка корзины пользователем и настройка trash.purge

lane: contracts
deps: [CTR-07, CTR-18]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md, crates/tp-testkit/src/*]
read: [docs/decisions/2026-11-01-trash-purge.md, docs/CONTRACTS.md#Trash, Retention, Quota (Ф1)]
checks: [nextest]
manual: no

## Сделать
- `TrashStore::purge(scope, ids) -> PurgeOutcome { hidden, erased }`: по действующей настройке `hide | erase`, Legal Hold → `hide`.
- Настройка `trash.purge` (организация: значение + `user_can_choose`; пользователь) и её вычисление в `PolicyResolver` (администратор приоритетнее).
- Скрытые письма: не видны в списках пользователя, не в квоте, видны администратору (`list_hidden`, `restore`).
- In-memory реализация и общий набор `suite::trash_purge` в tp-testkit — в том же PR (ветка `x/contracts-14`).

## Приёмка
- Набор: `hide` скрывает и не считает в квоте; `erase` стирает; Legal Hold не даёт стереть; пользователь не меняет значение без `user_can_choose`.
