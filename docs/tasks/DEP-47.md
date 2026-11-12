# DEP-47: Helm: порты из конфигурации узла

lane: deploy
deps: []
class: A
files: [deploy/helm/thepost/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 18; решение 105.

## Сделать
- `ports.*.container` в `values.yaml` дублирует адреса слушателей config.yaml: порты контейнера и Service выводить из
  `config` значений (одно место), дублирующие ключи удалить.

## Приёмка
- `helm template` с изменённым портом слушателя даёт тот же порт в Service и контейнере.
