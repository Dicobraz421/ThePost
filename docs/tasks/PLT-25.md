# PLT-25: метрики пула СУБД и слушателя сигналов

lane: platform
deps: []
class: A
files: [crates/tp-platform/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Занятость пула (занято, ожидают, время ожидания — гистограмма), состояние слушателя LISTEN (решение 57);
  `application_name` слушателя, чтобы он был виден в `pg_stat_activity`.

## Приёмка
- Тест: при исчерпании пула растёт время ожидания.
