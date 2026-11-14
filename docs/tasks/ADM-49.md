# ADM-49: политика доверия в Admin API — маршруты уровней, проверка потолка, партнёры, CLI

lane: admin
deps: [CTR-93, ADM-44]
class: B
files: [crates/tp-admin/*, crates/tp-admin-client/*, crates/thepost/src/admin_cli.rs, console/src/api/openapi.json, console/src/api/schema.d.ts]
read: [docs/decisions/2026-11-11-stage2-followups.md#124, docs/tasks/CTR-93.md, docs/ADMIN-GUIDE.md#что-может-менять-администратор, crates/tp-admin/src/server.rs, crates/tp-admin/src/sieve.rs]
checks: [nextest, console]
manual: no

## Сделать
- Класс B: схема Admin API меняется — копия консоли `console/src/api/openapi.json` и `schema.d.ts`
  (`thepost admin openapi -o …`, затем `npm run gen:api`) в этом же PR.
- Маршруты: установка — `GET|PUT|DELETE /api/v1/trust` (администратор установки); организация —
  `GET|PUT|DELETE /api/v1/orgs/{org}/trust`; группа — `GET|PUT|DELETE /api/v1/orgs/{org}/groups/{group}/trust`
  (администратор организации и установки; `GET` — и оператор); действующая для учётной записи —
  `GET /api/v1/orgs/{org}/accounts/{account}/trust`. `GET` уровня — `{own, effective, ceiling, source, over_ceiling}`;
  `PUT` — замена политики уровня целиком (клетки, у организации — `partners`, `auto_forward_domains`); `DELETE` — сброс
  к умолчаниям уровня выше. Закреплённые клетки — в ответе с признаком `fixed`, в теле `PUT` — 400.
- Проверка потолка — `TrustPolicy::validate` по действующему потолку уровня выше: выше потолка — 422 problem+json,
  `detail` по-русски и `errors: [{function, peer, value, ceiling, level}]` («Новое приглашение, сосед: «авто» выше
  потолка «письмо с кнопкой» (закреплено)»). Домен-партнёр из принятых доменов своей организации — 422. Снижение
  потолка не отклоняется: значения ниже по иерархии выше нового потолка отдаются с `over_ceiling`.
- Каждое изменение и сброс — в журнал аудита: уровень, клетки старые и новые, партнёры добавленные и удалённые.
- Скрипт организации Sieve (`PUT /api/v1/orgs/{org}/sieve`): проверка профилем «админский» SIEVE-14 при сохранении —
  `vacation` запрещён, `redirect` на внешний адрес — только на домен, куда автопересылка открыта; ошибка — 422 со
  строкой и столбцом, как у скриптов пользователя.
- Автоответ: `vacation set` и маршрут автоответа принимают внешние тему и текст (`external_subject`,
  `external_text_body`, `external_html_body`, CTR-93); CLI — `--external-subject`, `--external-text`.
- Все операции — в `openapi_spec()` (консоль, CLI и текстовый интерфейс, CI-16). CLI —
  `thepost admin trust show|set --file|reset [--org <орг>] [--group <группа>] [--account <учётка>]`,
  `trust partners list|set <домен>…`; `tp-admin-client` — методы этих маршрутов для TUI-09.

## Как делают другие
- Партнёры и удалённые домены — отдельные команды администратора: `New-OrganizationRelationship`, `Set-RemoteDomain`,
  `Set-SharingPolicy` (https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/new-organizationrelationship,
  https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-remotedomain,
  https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-sharingpolicy).
- Потолок администратора с выбором пользователя в его пределах — Google
  (https://knowledge.workspace.google.com/admin/calendar/automatically-add-events-to-calendars).
- Отличие: значение выше потолка не сохраняется — ответ с перечнем нарушенных клеток и уровнем, откуда потолок.

## Приёмка
- Тесты: `PUT` организации с `invite_new.partner = auto` — 200 и запись аудита; `invite_new.external = auto` — 422 с
  полями `errors`; `booking` в теле — 400; группа шире организации — 422; снижение потолка установки — организация
  отдаёт `over_ceiling`.
- Права: администратор организации не меняет уровень установки и чужую организацию (403/404); оператор — только `GET`.
- Скрипт организации с `vacation` или `redirect "x@gmail.com"` при закрытой автопересылке — 422 со строкой и столбцом.
- `GET …/accounts/{account}/trust` — значение и `source` по каждой клетке для учётной записи в двух группах.
