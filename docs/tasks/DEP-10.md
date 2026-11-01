# DEP-10: HPA, VPA и эталон «малый Enterprise»

lane: deploy
deps: [DEP-09]
class: A
files: [deploy/*]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/spec/S2.md#2.3, docs/spec/APP-Z.md]
checks: [nextest]
manual: docs/manual-tests/DEP-10.md

## Сделать
- Helm: HPA по CPU, по соединениям — опция (prometheus-adapter), окно стабилизации сжатия, PodDisruptionBudget, VPA в режиме рекомендаций.
- Ansible: эталон «малый Enterprise» — 2 узла `mailbox`, pg_auto_failover (primary, standby, монитор), SeaweedFS на трёх ВМ данных с метаданными filer в PostgreSQL.

## Приёмка
- `helm template` и проверка схем манифестов; ручная проверка отказа одной ВМ на стенде.
