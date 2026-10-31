# CLU-10: состав кластера и разделы по сигналу

lane: cluster
deps: [CTR-22]
class: A
files: [crates/tp-cluster/src/*]
read: [docs/decisions/2026-11-01-interaction-db-ha.md, docs/spec/S2.md#2.8]
checks: [nextest]
manual: no

## Сделать
- `PartitionAssigner::members()` обновляется по сигналу `MembersChanged` и при `Resync`; периодический опрос состава удалить.
- Истечение записи узла — единственный таймер (аренда).

## Приёмка
- Тест: вход и выход узла меняют назначение разделов без ожидания периода.
