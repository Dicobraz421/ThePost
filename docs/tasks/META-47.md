# META-47: DavStore на PostgreSQL

lane: meta
deps: [META-46]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `impl DavStore for PgMeta`: коллекции, объекты, условия ETag в одной транзакции, конфликт UID — `Conflict`, лимиты из секции `meta`.
- `DavChanged` — после фиксации (как `MailboxChanged`).

## Приёмка
- Набор `suite::dav` (CTR-45) на PostgreSQL при `TEST_DATABASE_URL`.
