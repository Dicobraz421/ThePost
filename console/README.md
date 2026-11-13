# Веб-консоль администратора The Post

Одностраничное приложение на [Refine](https://refine.dev) с Ant Design (SRS 5.2). Работает только через Admin API,
своей логики не содержит. Сборка `console/dist` вшивается в бинарный файл `thepost` (фича `console` у `tp-admin`,
DEPS-05) и раздаётся ролью admin под `/console/` (ADM-15). Внешние ресурсы (CDN, шрифты, телеметрия) не используются.

## Запуск с узлом разработчика

Нужны Node 26 (LTS, как в CI) и запущенный узел с ролью admin (`docs/DEV.md`).

```bash
npm ci
NODE_EXTRA_CA_CERTS=/путь/к/ca.pem THEPOST_ADMIN=https://127.0.0.1:8443 npm run dev
```

Консоль — `http://localhost:5173/console/`; запросы `/api/*` сервер разработки передаёт узлу (`THEPOST_ADMIN`,
по умолчанию `https://127.0.0.1:8443`: Admin API слушает только TLS, `http.admin.listen`). Сертификат узла
проверяется: CA, которым он выпущен, — в `NODE_EXTRA_CA_CERTS` (стандартная переменная Node). Вход — одноразовый
токен: `thepost admin init` на узле.

## Консоль на отдельном веб-сервере

Кроме раздачи узлом (`/console/`), консоль ставится архивом `console/dist` из выпуска на любой веб-сервер
(решение 68 от 11.11). Сборка рассчитана на адрес `/console/`: архив распаковывается в каталог `console`, неизвестные
пути отдаются `index.html` (маршруты приложения).

Адрес Admin API — файл `config.json` рядом с `index.html` (необязательный, читается один раз при открытии консоли):

```json
{ "apiBase": "https://mail.example.org:8443" }
```

`apiBase` — источник `https://узел[:порт]` без пути. Нет файла или поля — запросы идут к источнику страницы (как при
раздаче узлом). Вход через OIDC не меняется: поставщика сообщает узел (`GET /api/v1/session`), адрес возврата —
`<адрес консоли>/login`, он регистрируется у поставщика.

Пример nginx (архив распакован в `/srv/www/console`):

```nginx
server {
    listen 443 ssl;
    server_name console.example.org;
    # ssl_certificate …; ssl_certificate_key …;
    root /srv/www;
    location = / { return 302 /console/; }
    location /console/ {
        try_files $uri /console/index.html;
        add_header Content-Security-Policy "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self' https://mail.example.org:8443 https://idp.example.org; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'" always;
    }
    location /console/assets/ { expires 1y; }
}
```

- `connect-src` политики содержимого — адрес API из `apiBase` и, если вход через OIDC, поставщик; без них браузер
  не пропустит запросы. Политику ставит веб-сервер; `admin.console.connect_src` узла действует только на консоль,
  которую раздаёт сам узел.
- На узле — источник консоли в `admin.console.cors_origins`, иначе браузер не пропустит ответы Admin API:

  ```yaml
  admin:
    console:
      cors_origins: ["https://console.example.org"]
  ```

## Скрипты (их запускает job `console` в CI)

| Команда | Что делает |
|---|---|
| `npm run typecheck` | проверка типов TypeScript |
| `npm run lint` | ESLint |
| `npm test` | тесты `vitest` на заглушке API |
| `npm run build` | сборка в `console/dist` |
| `npm run gen:api` | типы `src/api/schema.d.ts` из `src/api/openapi.json` |

## Схема Admin API

`src/api/openapi.json` — схема Admin API узла (`GET /api/openapi.json`). После изменения API: выгрузить её без
запуска узла — `thepost admin openapi --output console/src/api/openapi.json` (APP-37; из корня репозитория:
`cargo run -p thepost -- admin openapi --output console/src/api/openapi.json`; на Windows `thepost` не собирается —
в Linux или WSL), выполнить `npm run gen:api`; тест
сверяет типы со схемой. Типы операций (`operations` в `schema.d.ts`) названы по `operationId` — они уникальны (ADM-27),
тест это проверяет. Каждая операция схемы — экран консоли (`src/api/coverage.ts`) или строка в `coverage-pending.txt`
(новая операция Admin API без экрана и без такой строки — красный тест).

## Роли

Меню и кнопки изменения показываются по ролям из `GET /api/v1/whoami` (`src/providers/access.ts`): аудитор видит
только журнал аудита, оператор — всё, кроме него, без кнопок изменения. Права проверяет узел. Ручная проверка
разделов по ролям — `docs/manual-tests/UI-09.md`.
