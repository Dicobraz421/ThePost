# CTR-85: очередь исходящей почты — список записей и повтор доставки в контракте `QueueStore`

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/queue.rs, docs/CONTRACTS.md]
read: [crates/tp-contracts/src/queue.rs, docs/tasks/TUI-01.md]
checks: [nextest]
manual: no

## Сделать
- Раздел «Очередь» в консоли и текстовом интерфейсе (TUI-01) нечем наполнить: у `QueueStore` нет чтения записей.
- `QueueStore::list(tenant, filter, cursor, limit)` — записи очереди организации порциями: отправитель, получатели со
  статусами, число попыток, ближайший срок, последняя ошибка, размер; фильтр по статусу и адресу.
- `QueueStore::retry(tenant, id)` — срок попытки «сейчас» у всех незавершённых получателей и сигнал `QueueReady`; нет
  записи — `Error::NotFound`; запись под лизой — без изменений, не ошибка.
- Тест набора в `tp-testkit`; реализации (`tp-meta`, `tp-testkit`) — отдельными волнами после контракта.

## Приёмка
- Тест набора: запись видна в `list` с фильтром; `retry` переносит срок и будит `next_due`; чужая организация — пусто.
