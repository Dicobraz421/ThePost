# META-72: очередь: `next_due` и сигнал в PostgreSQL

lane: meta
deps: [CTR-73, CTR-74]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/tasks/CTR-73.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 74–75: smtp-07, PR #548.

## Сделать
- `next_due` одним запросом, сигнал после фиксации постановки и переноса; постановка на шард отправителя (CTR-74).

## Приёмка
- Наборы CTR-73, CTR-74 на PostgreSQL.
