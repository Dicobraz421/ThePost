# ADM-48: брендинг заказчика в Admin API — чтение, запись, сброс, знак, контраст, открытый маршрут

lane: admin
deps: [CTR-91, DES-01, ADM-44]
class: B
files: [crates/tp-admin/*, crates/tp-admin-client/*, crates/thepost/src/admin_cli.rs, Cargo.toml, Cargo.lock, console/src/api/openapi.json, console/src/api/schema.d.ts]
read: [docs/decisions/2026-11-11-stage2-followups.md#122, docs/tasks/CTR-91.md, design/tokens.toml, crates/tp-admin/src/server.rs, crates/tp-admin/src/extract.rs]
checks: [nextest, console]
manual: no

## Сделать
- Класс B: схема Admin API меняется — копия консоли `console/src/api/openapi.json` и `schema.d.ts`
  (`thepost admin openapi -o …`, затем `npm run gen:api`) в этом же PR, иначе сверка схемы в CI падает; зависимости
  `palette` (WCAG-контраст, сдвиг светлоты в OKLCH) и `image` (только `png`, с `Limits`) — в
  `[workspace.dependencies]` версиями, что уже в `Cargo.lock` (дерево `ratatui`, `ratatui-image`); новых крейтов нет.
- Уровень установки — `GET|PUT|DELETE /api/v1/branding` (администратор установки), знак — `PUT|DELETE
  /api/v1/branding/logo`. Уровень организации — `GET|PUT|DELETE /api/v1/orgs/{org}/branding` и `PUT|DELETE
  /api/v1/orgs/{org}/branding/logo` (администратор организации и установки; `GET` — и оператор). `PUT` — тело
  `{product_name, primary, accent}` (знак сохраняется); `GET` — `{own, effective}` с цветами ролей; `DELETE` — сброс
  уровня. Каждое изменение и сброс — в журнал аудита (уровень, старые и новые значения, у знака — хэш, без байтов).
- Знак — тело `image/png` (иной тип — 415), больше 256 КБ — 413; не PNG, битый файл, больше 1024×1024 — 422. PNG
  проверяется полным разбором `image` с `Limits`; байты — `BlobStore::put_raw(BrandLogo::path())` (уже есть — не
  писать). Знак без названия и цветов уровня — 409. Прежний знак после замены или сброса удаляется
  (`delete_raw`), если `logo_in_use` — `false`.
- Цвета ролей (`branding.rs`, по темам `light` и `dark`; фоны `screen` и `panel` — из `design/tokens.toml`,
  `include_str!` и `toml`, второго списка цветов нет): `sel`, `button` — акцент; `fkBg`, `frameA` — основной; текст на
  них (`selFg`, `buttonFg`, `fkFg`) — белый или чёрный, у которого контраст выше. Пороги — пары `[[contrast]]` файла
  токенов с этими ролями (`min`, включая исключение `selFg`). Не проходит — светлота цвета роли сдвигается в OKLCH (в
  тёмной теме светлее, в светлой темнее) не больше чем на 0,2; не прошло и так — 422 problem+json: `detail` по-русски
  и `errors: [{field, role, theme, contrast, min}]` («акцент #F5E663: выделение на белой панели светлой темы —
  1,3:1, после сдвига 2,5:1, нужно 3:1»). Формат цвета — 400.
- Открытый маршрут без входа — `GET /api/v1/public/branding?domain=<домен>` и
  `GET /api/v1/public/branding/logo/{hash}`: только название, цвета ролей и ссылка на знак (или `null` — встроенный
  IT TASK), без имени, идентификатора и уровня организации. Организация — по принятому домену
  (`TransportStore::cluster`); неизвестный домен, организация без своего набора и запрос без `domain` — один и тот же
  ответ (брендинг установки). `ETag`, `Cache-Control: public, max-age=300`; знак по хэшу — `immutable`. Защита от
  перебора — общее ограничение частоты Admin API по адресу клиента.
- Все операции — в `openapi_spec()` (их подхватят консоль, CLI и текстовый интерфейс, CI-16). CLI —
  `thepost admin branding show|set|reset|logo set <файл.png>|logo reset [--org <орг>]`; `tp-admin-client` —
  `public_branding(domain)` и `branding_logo(hash)` для экрана входа текстового интерфейса (TUI-08).

## Приёмка
- Тесты: IT TASK (`#177CAA`, `#E4107F`) проходит обе темы; `#F5E663` акцентом — 422 с полями `errors`; `#FFF` — 400;
  знак: JPEG — 415, 300 КБ — 413, 2048×16 — 422, годный PNG — 200 и запись аудита; знак без набора — 409.
- Права: администратор организации не меняет уровень установки и чужую организацию (403/404); оператор — только `GET`.
- Открытый маршрут: ответы для неизвестного домена, домена организации без своего набора и без `domain` побайтно
  равны; домен организации со своим набором — её цвета; в ответе нет полей организации; `If-None-Match` — 304.
- Сброс организации возвращает брендинг установки; знак, не нужный больше ни одному уровню, удалён из BLOB.
