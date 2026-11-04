# META-34: хранение публикаций календарей и шаблонов партнёров

lane: meta
deps: [CTR-36, META-33]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*]
read: [docs/decisions/2026-11-04-calendar-external-api.md, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

## Сделать
- Таблица ссылок публикации (токен — только хэш, уровень, срок, отзыв) с RLS; шаблоны ссылок занятости доменов-партнёров в политике организации.

## Приёмка
- Набор conformance testkit на PostgreSQL (при `TEST_DATABASE_URL`).
