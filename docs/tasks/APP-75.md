# APP-75: узел — брендинг заказчика (`BrandingStore` и BLOB в Admin API)

lane: app
deps: [META-84, ADM-48]
class: A
files: [crates/thepost/src/*, crates/thepost/tests/*]
read: [docs/tasks/ADM-48.md, docs/tasks/META-84.md]
checks: [nextest]
manual: no

## Сделать
- Admin API узла получает `BrandingStore` — `PgMeta` — и `BlobStore` узла для знаков; память и заглушки в узле не
  остаются.
- Открытый маршрут `GET /api/v1/public/branding` проходит без сессии и под общим ограничением частоты, как вход по
  паролю; остальные маршруты брендинга — с сессией.

## Приёмка
- e2e узла: набор установки и знак через Admin API → открытый маршрут отдаёт их без входа; свой набор организации —
  по её домену; после перезапуска узла набор и знак на месте; сброс организации — снова брендинг установки.
