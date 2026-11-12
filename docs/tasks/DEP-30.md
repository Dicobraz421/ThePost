# DEP-30: эталоны: прямые адреса и реплики СУБД

lane: deploy
deps: [META-41, META-32]
class: A
files: [deploy/ansible/*, deploy/helm/thepost/*, deploy/config/*, deploy/README.md]
read: [docs/tasks/META-41.md, docs/tasks/META-32.md]
checks: [none]
manual: no

Хвосты волн meta-25 (PR #403: «`direct_urls` описать волной полосы deploy») и meta-14 (PR #401: реплика для фоновых задач).

## Сделать
- `pg-ha.yml`, `small-enterprise.yml`, Helm: `database.direct_urls` (адреса мимо PgBouncer — слушатель сигналов и
  миграции), `database.read_replica` и `replica_max_lag`; `deploy/README.md` — когда что задавать.

## Приёмка
- `--syntax-check`, `helm template` без ошибок; сгенерированный config.yaml проходит `thepost config check`.
