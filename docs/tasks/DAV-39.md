# DAV-39: удалить `tp_dav::router_with`, тесты — на `router_shared`

lane: dav
deps: []
class: A
files: [crates/tp-dav/src/server.rs, crates/tp-dav/src/lib.rs, crates/tp-dav/src/tests/*]
read: [docs/tasks/APP-71.md]
checks: [nextest]
manual: no

## Сделать
- После #764 узел вызывает `router_shared`; `router_with` остался только в тестах крейта (`tests/schedule.rs` и др.).
  Удалить функцию и её описания; тесты перевести на
  `router_shared(deps, schedule, None, cfg)` или со `Some(SharingStores)`, где проверяется общий доступ. Параллельный
  путь не оставлять.

## Приёмка
- Поиск `router_with` по `crates` и `docs` пуст (кроме истории); тесты `tp-dav` проходят без смены проверок.
