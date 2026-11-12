# META-53: метрика доли NOTIFY во времени фиксации primary

lane: meta
deps: []
class: A
files: [crates/tp-meta/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Метрика публикатора сигналов: время фиксаций с NOTIFY и общее время фиксаций (гистограммы), доля — в `/metrics`;
  число соединений-слушателей (решение 57, Д.7 строки 6–7).

## Приёмка
- Тест публикатора на PostgreSQL: метрики растут при сигналах.
