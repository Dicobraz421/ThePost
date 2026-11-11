# ADM-15: раздача веб-консоли ролью admin

lane: admin
deps: [DEPS-05, UI-01]
class: A
files: [crates/tp-admin/src/*]
read: [docs/spec/S5.md#5.2, docs/decisions/2026-11-01-scale-shards-geo-admin.md]
checks: [nextest]
manual: no

## Сделать
- `GET /console/*` — встроенная статика (`rust-embed`, фича `console`), неизвестный путь — `index.html` (маршруты SPA);
  `Content-Security-Policy` без внешних источников, `X-Frame-Options: DENY`, кэширование хэшированных файлов; `/` → `/console/`.
- Без фичи — страница «консоль не собрана» со ссылкой на `docs/DEV.md`; `admin.console.enabled` (по умолчанию — да).

## Приёмка
- Тесты: `index.html` и файл ресурса, маршрут SPA, заголовки безопасности, выключенная консоль — 404.
