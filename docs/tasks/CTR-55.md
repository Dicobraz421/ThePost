# CTR-55: очередь модерации — продолжение по курсору

lane: contracts
deps: [CTR-43]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md#Groups, docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

Хвост волны console-02, PR #439.

## Сделать
- `GroupStore::held_after(tenant, group, after, limit)`: страница очереди модерации по (`held_at`, `id`) строго после
  курсора `after` (пара `held_at` и `HeldId` последнего письма предыдущей страницы; `None` — с начала). `held` остаётся
  (первая страница). По умолчанию — `Error::Internal` (реализации — `tp-meta`, `tp-testkit`).
- Реализация в памяти, общий тест набора, `docs/CONTRACTS.md#Groups`, CHANGELOG. Потребители: META-59, ADM-21.

## Приёмка
- CI зелёный; тест набора: очередь из 5 писем читается страницами по 2 без пропусков и повторов, в том числе при
  одинаковом `held_at`.
