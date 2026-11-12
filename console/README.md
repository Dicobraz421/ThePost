# Веб-консоль администратора The Post

Одностраничное приложение на [Refine](https://refine.dev) с Ant Design (SRS 5.2). Работает только через Admin API,
своей логики не содержит. Сборка `console/dist` вшивается в бинарный файл `thepost` (фича `console` у `tp-admin`,
DEPS-05) и раздаётся ролью admin под `/console/` (ADM-15). Внешние ресурсы (CDN, шрифты, телеметрия) не используются.

## Запуск с узлом разработчика

Нужны Node 26 (LTS, как в CI) и запущенный узел с ролью admin (`docs/DEV.md`).

```bash
npm ci
THEPOST_ADMIN=http://127.0.0.1:8080 npm run dev
```

Консоль — `http://localhost:5173/console/`; запросы `/api/*` сервер разработки передаёт узлу (`THEPOST_ADMIN`,
по умолчанию `http://127.0.0.1:8080`). Вход — одноразовый токен: `thepost admin init` на узле.

## Скрипты (их запускает job `console` в CI)

| Команда | Что делает |
|---|---|
| `npm run typecheck` | проверка типов TypeScript |
| `npm run lint` | ESLint |
| `npm test` | тесты `vitest` на заглушке API |
| `npm run build` | сборка в `console/dist` |
| `npm run gen:api` | типы `src/api/schema.d.ts` из `src/api/openapi.json` |

## Схема Admin API

`src/api/openapi.json` — копия `GET /api/openapi.json` узла. После изменения API: скачать схему с узла
(`curl -s $THEPOST_ADMIN/api/openapi.json > src/api/openapi.json`), выполнить `npm run gen:api`; тест сверяет типы
со схемой. Каждая операция схемы — экран консоли (`src/api/coverage.ts`) или строка в `coverage-pending.txt`
(список пуст с UI-09: новая операция Admin API без экрана — красный тест).

## Роли

Меню и кнопки изменения показываются по ролям из `GET /api/v1/whoami` (`src/providers/access.ts`): аудитор видит
только журнал аудита, оператор — всё, кроме него, без кнопок изменения. Права проверяет узел. Ручная проверка
разделов по ролям — `docs/manual-tests/UI-09.md`.
