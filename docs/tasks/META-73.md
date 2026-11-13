# META-73: GC: кандидаты после отбора занятых, ближайший срок одним запросом

lane: meta
deps: [BLOB-15]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/tasks/BLOB-15.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 82: blob-08, PR #527. `gc_candidates` (`blob_index.rs`) ограничивает выборку до отбрасывания занятых записей —
кандидаты за ними не видны; ближайший срок — около 30 запросов на шард на взвод.

## Сделать
- Отбор занятых — в запросе до `LIMIT`; `next_gc_due` — `MIN(zero_since)` одним запросом на шард.

## Приёмка
- Тест PostgreSQL: 100 занятых записей впереди не скрывают свободного кандидата; `next_gc_due` — один запрос.
