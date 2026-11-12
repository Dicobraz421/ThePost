# DAV-29: CardDAV: книга GAL из `DavStore`

lane: dav
deps: [CTR-60, DAV-27, DAV-28]
class: A
files: [crates/tp-dav/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 72: GAL — один раз в хранилище, одинаково для CardDAV и JMAP.

## Сделать
- `tp_dav::gal::sync(directory, dav, tenant)`: карточки из каталога (учётные записи и группы, без скрытых — CTR-58)
  строит прежний код `gal.rs`, записывает `DavStore::sync_gal`; вызов — при старте роли и по событию изменения каталога
  (подключение — APP-49).
- Отдача книги «Организация» — из `DavStore` как обычной книги только для чтения; кэш `gal::Cache` и свой токен
  синхронизации — убрать.

## Приёмка
- Тесты на in-memory: состав книги после `sync`; изменение каталога — новая версия одной карточки, остальные ETag
  прежние; запись в книгу — 403.
