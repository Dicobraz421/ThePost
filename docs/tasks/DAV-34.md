# DAV-34: `tp_dav::router_with` без аргумента `_gal`

lane: dav
deps: [DAV-29, APP-49]
class: B
files: [crates/tp-dav/src/server.rs, crates/tp-dav/src/tests/schedule.rs, crates/thepost/src/services.rs]
read: [docs/tasks/DAV-29.md, docs/tasks/APP-49.md]
checks: [nextest]
manual: no

## Сделать
- После DAV-29 (#668) и APP-49 (#669) книгу GAL маршруты читают из хранилища, заполняет её `gal::sync`; аргумент
  `_gal: Option<GalDeps>` у `router_with` не используется, узел передаёт `None` — след старого пути.
- Убрать аргумент из сигнатуры и всех вызовов: тест планирования в tp-dav и `Services` в `thepost`.

## Приёмка
- `git grep -n router_with` — вызовы с тремя аргументами `(deps, schedule, cfg)`; сборка и тесты зелёные в CI.
