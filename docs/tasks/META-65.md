# META-65: каталог `Directory` поверх учётных записей PostgreSQL

lane: meta
deps: [CTR-60]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/CONTRACTS.md#Directory, docs/tasks/DAV-29.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 43: app-28, PR #484. без LDAP реализации `Directory` нет — книги «Организация» у SMB-пилота без каталога нет.

## Сделать
- `impl Directory for PgMeta`: `accounts_since`, `groups_since`, `aliases`, `domains`, `gal_attributes` — из локальных
  учётных записей, групп и псевдонимов (версия — `ModSeq`/время изменения строк); скрытые из GAL (CTR-58) — с признаком.

## Приёмка
- Набор `suite::directory` на PostgreSQL; учётная запись, созданная через Admin API, видна в `accounts_since`.
