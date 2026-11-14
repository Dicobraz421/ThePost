# META-84: брендинг заказчика в PostgreSQL — миграция 0043 и `BrandingStore` в `PgMeta`

lane: meta
deps: [CTR-91]
class: A
files: [crates/tp-meta/*]
read: [docs/tasks/CTR-91.md, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Миграция **0043** (0041 — META-81, 0042 — META-83): таблица брендинга — по строке на уровень: установка
  (`tenant IS NULL`, единственность — частичным уникальным индексом) и организация (уникальный `tenant`); название,
  `primary` и `accent` (`CHECK` на `^#[0-9A-F]{6}$`), знак — хэш, размер, ширина и высота (все четыре заданы или все
  `NULL`), время изменения. Байты знака — в BLOB (`BrandLogo::path()`), не в таблице.
- `BrandingStore` в `PgMeta` по контракту CTR-91: `effective_branding` — одним запросом (организация, иначе установка);
  `logo_in_use` — по индексу хэша.

## Приёмка
- Набор `tp-testkit` `suite::branding` проходит на `PgMeta` (PostgreSQL) наравне с памятью.
- Тест миграции: таблица создаётся поверх 0042, повторный запуск ничего не меняет; вторая строка уровня установки —
  ошибка ограничения.
