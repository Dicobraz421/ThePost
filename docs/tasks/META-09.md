# META-09: QueueStore на PostgreSQL

lane: meta
deps: [CTR-06, META-05]
class: B
files: [crates/tp-meta/src/queue.rs, crates/tp-meta/migrations/*]
read: [docs/CONTRACTS.md#Queue (Ф1)]
checks: [nextest]
manual: no

## Сделать
- Таблицы очереди и получателей, claim с лизой, расписание повторов, статусы.

## Приёмка
- Конкурентный тест claim; повтор по времени.
