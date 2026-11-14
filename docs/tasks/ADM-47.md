# ADM-47: тип ресурса бронирования в Admin API и CLI

lane: admin
deps: [CTR-88, ADM-44]
class: A
files: [crates/tp-admin/*, crates/thepost/src/admin_cli.rs]
read: [docs/tasks/CTR-88.md, crates/tp-admin/src/resources.rs]
checks: [nextest]
manual: no

## Сделать
- `resource_type` (`room` | `equipment`) в `ResourceInfo` и `NewResource` (нет в запросе — `room`).
- Смена типа — `PATCH /api/v1/orgs/{org}/resources/{account}` с телом `{"resource_type": "equipment"}`; запись в журнал
  аудита; неизвестное значение — 400 problem+json. Операция — в `openapi_spec()`: её подхватят консоль, CLI и
  текстовый интерфейс (CI-16).
- CLI: `thepost admin resources create --type room|equipment`, `thepost admin resources set-type <адрес> <тип>`; колонка
  «Тип» в `resources list`.

## Приёмка
- Тесты: создание без типа — `room`; создание с `equipment`; `PATCH` меняет тип и пишет аудит; чужая организация — 404;
  неизвестный тип — 400.
