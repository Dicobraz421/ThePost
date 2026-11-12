# META-58: перенос объекта DAV и индекс поиска контактов

lane: meta
deps: [CTR-54, META-46]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*]
read: [docs/CONTRACTS.md#Dav]
checks: [nextest]
manual: no

Хвост волны jmap-16, PR #428.

## Сделать
- `DavStore::move_object` (CTR-54): одна транзакция — запись в целевой коллекции с прежним `id`, надгробие в исходной,
  `ModSeq` обеих коллекций, событие `DavChanged`.
- `DavStore::find_cards`: поисковые строки контакта хранятся при `put_object` (миграция), поиск — по строкам индекса
  учётной записи без разбора vCard; только встроенные средства PostgreSQL (без расширений — `docs/spec/S3.md`).
  Строки существующих контактов заполняются при миграции или лениво при первой записи — выбрать и описать в PR.

## Приёмка
- Общий набор тестов CTR-54 проходит на PostgreSQL; план запроса `find_cards` не читает тела объектов.
