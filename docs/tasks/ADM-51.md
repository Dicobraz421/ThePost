# ADM-51: каталог признаков и TI в Admin API — признаки, пороги, пакеты, источники, срабатывания; CLI

lane: admin
deps: [CTR-95, ADM-44]
class: B
files: [crates/tp-admin/*, crates/tp-admin-client/*, crates/thepost/src/admin_cli.rs, console/src/api/openapi.json, console/src/api/schema.d.ts]
read: [docs/decisions/2026-11-11-stage2-followups.md#126, docs/tasks/CTR-95.md, docs/tasks/ADM-49.md, crates/tp-admin/src/server.rs]
checks: [nextest, console]
manual: no

## Сделать
- Класс B: схема Admin API меняется — копия консоли `console/src/api/openapi.json` и `schema.d.ts`
  (`thepost admin openapi -o …`, затем `npm run gen:api`) в этом же PR.
- Признаки: `GET /api/v1/risk/signals` и `GET /api/v1/orgs/{org}/risk/signals` — действующий каталог: по признаку
  `{id, origin, detector, weight, threshold, action, mode, since, source, orphaned}`; `PUT|DELETE
  /api/v1/risk/signals/{id}` и `…/orgs/{org}/risk/signals/{id}` — изменение и сброс (свои признаки `org.*` — создание
  тем же `PUT`). Организация мягче установки — 422 problem+json, `detail` по-русски, `errors: [{signal, field, value,
  base}]`. Пороги — `GET|PUT /api/v1/risk/thresholds` (и уровня организации).
- Пакеты (администратор установки): `GET /api/v1/risk/packages` — активный, прошлые, отклонённые с отчётом;
  `POST /api/v1/risk/packages` — загрузка файла пакета (`application/octet-stream`, офлайн-импорт; предел размера —
  `admin.risk.max_package_bytes`) → 202 и отчёт проверки (подпись, lint, пробный прогон, `UpdateReport`);
  `POST /api/v1/risk/packages/{version}/activate`, `POST /api/v1/risk/packages/{version}/rollback`. Сама проверка и
  установка — интерфейс, который передаёт узел (APP-78); в тестах — память.
- Источники TI: `GET|POST /api/v1/risk/sources`, `GET|PUT|DELETE /api/v1/risk/sources/{id}` (вид, формат, TTL, действие
  при устаревании, лицензия, организации MSSP, ключ — только `SecretRef`, значение ключа API не принимает и не
  отдаёт); `POST …/{id}/import` — файл списка; `POST …/{id}/refresh` — обновить сейчас; `GET …/{id}/usage?since=` —
  учёт по организациям. `Bundled` через API не создаётся — только пакетом (400).
- Срабатывания: `GET /api/v1/orgs/{org}/risk/hits?signal=&mode=&since=` — постранично; оператор — только `GET`.
- Каждое изменение признака, порога, источника, установка, активация и откат пакета — в журнал аудита.
- Все операции — в `openapi_spec()` (CI-16). CLI — `thepost admin risk signals list|set|reset [--org]`,
  `risk thresholds show|set`, `risk package list|import <файл>|activate|rollback <версия>`,
  `risk source list|add|set|rm|import <файл>|refresh|usage`, `risk hits [--signal] [--since]`; `tp-admin-client` —
  методы этих маршрутов для TUI-10.

## Как делают другие
- Баллы и правила меняются через веб-интерфейс и API: Rspamd controller (`/actions`, `/maps`,
  https://docs.rspamd.com/developers/protocol), Stalwart — настройки спам-фильтра в веб-админке
  (https://stalw.art/docs/spamfilter/settings/); установка пакета из файла — `sa-update --install`
  (https://spamassassin.apache.org/full/4.0.x/doc/sa-update.html).
- Отличие: установка пакета — только после отчёта проверки, активация — отдельным действием или автоматически по
  конфигурации узла; ключи фидов — ссылкой на хранилище секретов.

## Приёмка
- Тесты: `PUT` признака установки — 200 и запись аудита; организация снижает вес — 422 с `errors`; `POST` пакета с
  чужой подписью или меньшим номером — 422 с причиной; `activate` и `rollback` — 200, повтор `rollback` на
  неизвестную версию — 404.
- Источник с ключом-значением в теле — 400; `Bundled` в `POST` — 400; `usage` — суммы по организациям.
- Права: администратор организации не трогает пакеты, источники и уровень установки (403); оператор — только `GET`.
