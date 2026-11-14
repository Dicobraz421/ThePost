# META-82: просмотр очереди и повтор доставки в PostgreSQL — `QueueStore::list` и `retry`

lane: meta
deps: [CTR-85]
class: A
files: [crates/tp-meta/src/queue.rs, crates/tp-meta/tests/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, crates/tp-contracts/src/queue.rs]
checks: [nextest]
manual: no

## Сделать
- `QueueStore::list` и `QueueStore::retry` в `PgMeta` по контракту CTR-85 (#760): выборка по `QueueFilter` порциями
  `after` и `limit` в порядке `QueueId`; `retry` ставит срок попытки `now` получателям без конечного статуса, запись
  с действующей лизой не меняет и возвращает `false`, нет записи — `Error::NotFound`.
- Миграция не нужна: статусы и лиза уже лежат в `queue` и `queue_rcpts`.

## Приёмка
- Набор `tp-testkit` `suite::lifecycle::queue_list` проходит на `PgMeta` (PostgreSQL) наравне с `MemQueue`: фильтры,
  порции без пропусков и повторов, `retry` по записи с лизой и без неё, чужая организация — `NotFound`.
