# META-46: схема календарей и контактов

lane: meta
deps: [CTR-45]
class: B
files: [crates/tp-meta/migrations/*, crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/spec/S3.md#3.1, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Миграция: `dav_collections` (вид, имя, свойства, `modseq`), `dav_objects` (имя, UID, ETag, тело `text`, компонент, начало и конец
  интервала, `modseq`), `dav_tombstones` (имя, `modseq`, время); уникальность (коллекция, имя) и (коллекция, UID); индекс по интервалу
  для time-range; RLS по `tenant_id`; `ModSeq` — из счётчика учётной записи.
- Тела объектов — в PostgreSQL (SRS 3.1: «календари и контакты»), не в BLOB.

## Приёмка
- Миграция применяется на пустой и на текущей базе; RLS-тест (чужая организация не видит строк).
