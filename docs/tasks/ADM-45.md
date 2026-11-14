# ADM-45: Admin API очереди — `GET /orgs/{org}/queue`, `POST /orgs/{org}/queue/{id}/retry`

lane: admin
deps: [CTR-85]
class: A
files: [crates/tp-admin/*]
read: [docs/tasks/CTR-85.md]
checks: [nextest]
manual: no

## Сделать
- `GET /api/v1/orgs/{org}/queue` — записи очереди (`QueueStore::list`): фильтры `status`, `address`, листание по `Link`
  (`rel="next"`), как журнал аудита.
- `POST /api/v1/orgs/{org}/queue/{id}/retry` — повтор доставки (`QueueStore::retry`), 202; нет записи — 404 problem+json.
- Права — администратор организации; каждый повтор — в журнал аудита. Операции — в `openapi_spec()`: их подхватят CLI,
  консоль и текстовый интерфейс (CI-16).

## Приёмка
- Тесты: список с фильтром и второй страницей; повтор — 202 и запись аудита; чужая организация — 404.
