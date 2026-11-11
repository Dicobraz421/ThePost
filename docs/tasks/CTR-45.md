# CTR-45: календари и контакты — коллекции, объекты, журнал изменений

lane: contracts
deps: [CTR-03, CTR-04]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/spec/S4.md#4.4, docs/spec/S3.md#3.1, docs/CONTRACTS.md]
checks: [nextest]
manual: no

Основа Ф2: одно хранилище для CalDAV, CardDAV и JMAP Calendars/Contacts. Хранилище тела не разбирает — индекс
объекта вычисляет вызывающий (tp-dav, tp-jmap) библиотекой calcard.

## Сделать
- Модуль `dav` (раздел CONTRACTS.md#Dav): `CollectionId`, `DavObjectId`; `CollectionKind::{Calendar, AddressBook}`;
  `Collection { id, kind, name (сегмент URL), display_name, description, color, order, timezone, components, state: ModSeq }`
  (`components` — `VEVENT`, `VTODO` для календаря).
- `DavObject { id, collection, name (имя ресурса), uid, etag, body: String, index: ObjectIndex }`;
  `ObjectIndex { component, uid, span: Option<(start, Option<end>)> }` — интервал с развёрнутыми повторами, `None` у конца — бесконечный.
- `DavStore` (по `AccountScope`): коллекции — `list(kind)`, `get`, `create`, `update`, `delete` (сразу, без корзины; объекты — в надгробия);
  объекты — `get(collection, name)`, `by_uid(collection, uid)`, `list(collection) -> (name, etag)`, `multiget(names)`,
  `in_span(collection, span)` (кандидаты для time-range), `put(collection, name, body, index, Precondition::{Any, IfMatch(etag), IfNoneMatch})
  -> (etag, ModSeq)`, `delete(collection, name, Precondition)`. UID уникален в коллекции — повтор под другим именем `Conflict`.
- Изменения: `changes(collection, since, limit) -> Changes<String>` (имена; RFC 6578) и `account_changes(scope, kind, since, limit)`
  (JMAP `/changes`); `ModSeq` — из счётчика учётной записи (как у писем); устаревший `since` — `Conflict`.
- Событие `EventKind::DavChanged { kind, collection, modseq }` (учётная запись) после фиксации — JMAP push и кэши.
- Лимиты (размер объекта, объектов в коллекции, коллекций) — в конфигурации реализации; превышение — `QuotaExceeded`.
- `MemDav` в tp-testkit и общий набор `suite::dav` (его же прогоняет META-47 на PostgreSQL).

## Приёмка
- Набор `suite::dav` на in-memory: условия ETag, конфликт UID, надгробия, `changes` по коллекции и учётной записи, разделение организаций.
- CHANGELOG в CONTRACTS.md.
