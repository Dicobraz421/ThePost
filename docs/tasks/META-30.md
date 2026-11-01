# META-30: хранилища ящика через справочник шардов

lane: meta
deps: [META-29, META-27]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/spec/S3.md#3.6, docs/spec/S3.md#3.8, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Все хранилища ящика (папки, письма, журнал изменений, ACL, корзина, манифест индекса) получают пул через `ShardResolver::pool_for(scope, intent)`; один шард — без запроса к справочнику.
- Миграции схемы ящика — на всех шардах справочника; каталог — только в основной БД.
- Столбец `site` в справочнике шардов; изменение справочника — сигнал, пулы — лениво, кэш учётная запись → шард сбрасывается сигналом.

## Приёмка
- Тест на двух шардах (две базы одного PostgreSQL): ящики разных шардов обслуживаются через один `PgMeta`; данные не пересекаются.
