# ADM-24: CORS в Admin API для консоли на отдельном веб-сервере

lane: admin
deps: [ADM-15, DEPS-10]
class: A
files: [crates/tp-admin/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 68 (консоль из архива `console/dist` на любом веб-сервере; хвост console-01, PR #429).

## Сделать
- Настройка `admin.console.cors_origins` — список источников `https://узел[:порт]` (проверка как у `connect_src`),
  по умолчанию пуст — заголовков CORS нет (консоль с узла работает как сейчас).
- Для перечисленных источников: ответ на `OPTIONS` (preflight) и `Access-Control-Allow-Origin` у ответов `/api/v1/*`;
  разрешённые заголовки — `Authorization`, `Content-Type`, `If-Match`; `Access-Control-Allow-Credentials` не ставится
  (вход — заголовком `Authorization`, не cookie). Готовый слой — `tower-http` `CorsLayer` (DEPS-10).
- Чужой источник — ответ без заголовков CORS (браузер запрос не пропустит); в журнал — без адресов писем и токенов.

## Приёмка
- Тесты роутера: preflight с разрешённого источника — 204 с заголовками; с чужого — без них; пустая настройка — без
  заголовков; `/console/*` не затронут.
