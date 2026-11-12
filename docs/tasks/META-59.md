# META-59: очередь модерации — страница по курсору

lane: meta
deps: [CTR-55]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/CONTRACTS.md#Groups]
checks: [nextest]
manual: no

Хвост волны console-02, PR #439.

## Сделать
- `GroupStore::held_after` (CTR-55): выборка по (`held_at`, `id`) строго после курсора, по существующему индексу
  очереди; `held` — через неё же.

## Приёмка
- Общий тест набора CTR-55 проходит на PostgreSQL.
