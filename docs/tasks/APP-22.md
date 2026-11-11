# APP-22: CLI thepost admin без копий структур

lane: app
deps: [ADM-13, APP-21]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Решение владельца 2026-11-11, п. 15.

## Сделать
- `admin_client`: модуль `wire` (копии структур tp-admin) удалить — запросы и ответы прямо структурами tp-admin.
- `account list` без `--login` — постраничный список (`--prefix`, `--limit`, переход по `Link`), справка без
  «полного списка Admin API пока не отдаёт».

## Приёмка
- Тесты против роутера tp-admin зелёные; копий структур Admin API в thepost нет.
