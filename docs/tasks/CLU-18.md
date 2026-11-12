# CLU-18: идентичность узла — только URI `thepost:node:<id>`

lane: cluster
deps: [DEP-22]
class: A
files: [crates/tp-cluster/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/tasks/CLU-14.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 11; решение 103.

## Сделать
- `node_identity`: только SAN URI `thepost:node:<id>`; поиск по DNS/IP из `NodeInfo::addr` и CN — удалить;
  сертификат без URI — отказ с понятной ошибкой. Тестовые сертификаты — с URI.

## Приёмка
- Тесты: сертификат с URI принят, только DNS или только CN — отказ.
