# META-52: права на коллекции, представители, правила ресурсов

lane: meta
deps: [CTR-48]
class: B
files: [crates/tp-meta/migrations/*, crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Миграция: ACL коллекций, представители, `BookingPolicy` ресурса, политика общего доступа организации; RLS.
- Реализации CTR-48 на `PgMeta`; `shared_with` — одним запросом с раскрытием групп.

## Приёмка
- Наборы testkit CTR-48 на PostgreSQL при `TEST_DATABASE_URL`.
