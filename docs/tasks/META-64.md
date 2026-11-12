# META-64: сессии Admin API в PostgreSQL

lane: meta
deps: [CTR-61]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 77 (этап 3).

## Сделать
- Таблица сессий (миграция): хэш, субъект, роли, срок; индекс по сроку; `purge_expired` — пакетами.

## Приёмка
- Общий набор тестов CTR-61 на PostgreSQL (при `TEST_DATABASE_URL`).
