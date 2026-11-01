# META-29: очистка корзины и скрытые письма в PostgreSQL

lane: meta
deps: [CTR-26, META-10]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*]
read: [docs/decisions/2026-11-01-trash-purge.md]
checks: [nextest]
manual: no

## Сделать
- `PgMeta`: `TrashStore::purge`, признак скрытия, настройка `trash.purge` (организация и пользователь), квота без скрытых.

## Приёмка
- `suite::trash_purge` проходит на PostgreSQL.
