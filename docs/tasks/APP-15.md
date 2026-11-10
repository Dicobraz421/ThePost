# APP-15: сведения об узле в `heartbeat`

lane: app
deps: [META-27]
class: A
files: [crates/thepost/src/*]
read: []
checks: [nextest]
manual: no

Отчёт meta-10 (META-27): `serve.rs` продлевает запись через `heartbeat_roles` и не передаёт адрес и площадку.

## Сделать
- `heartbeat(&NodeInfo { id, roles, addr, site })`: адрес — внутренний адрес узла из конфигурации, площадка — `node.site`
  (по умолчанию `default`).
- Тест сборки сервисов: после старта `live_info()` возвращает узел с ролями и площадкой.

## Состояние (app-14)
- Сделано: `serve` продлевает запись `Membership::heartbeat(&NodeInfo)` — роли ядра, `cluster.addr`
  (пусто или `https://host:port`), `cluster.site` (по умолчанию `default`); роли модулей в запись не попадают
  (`NodeInfo::roles` — роли ядра). `Running::members` — членство; тесты `node_info_*`, `cluster_site_and_addr_are_checked`,
  `minimal_config_reaches_ready` (проверка `live_info()`, при `TEST_DATABASE_URL`).
