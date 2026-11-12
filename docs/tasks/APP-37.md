# APP-37: `thepost admin openapi` — выгрузка схемы Admin API без запуска узла

lane: app
deps: []
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

Хвост волны console-01, PR #429.

## Сделать
- Команда `thepost admin openapi [--output <файл>]`: печатает `tp_admin::server::openapi_spec()` в JSON со стабильным
  порядком ключей и переводом строки в конце — в stdout или файл; без `config.yaml`, сети и токена. Сейчас схему для
  `console/src/api/openapi.json` получают временной программой вне репозитория.
- Размещение — `cli.rs` или отдельный модуль, не `admin_client.rs` (его полоса меняется в CI-07).
- Сверка схемы консоли с узлом в CI — не здесь (п. 27 синка 12.11, ждёт решения владельца).

## Приёмка
- Тест: вывод команды совпадает с `openapi_spec()`; команда работает без конфигурации.
