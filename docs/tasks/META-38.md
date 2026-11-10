# META-38: `PgMeta` поверх `Failover` — повтор операций при переключении

lane: meta
deps: [META-25]
class: A
files: [crates/tp-meta/src/*]
read: [docs/decisions/2026-11-01-interaction-db-ha.md]
checks: [nextest]
manual: no

Отчёты meta-09 (META-25) и app-13 (APP-14): `PgMeta` получает голый `failover.pool()` — primary выбирается при запуске,
повтора идемпотентных операций и переключения на ходу нет.

## Сделать
- `PgMeta::from_failover(&Failover)`: временная ошибка соединения (отказ primary, 25006, обрыв) → сброс пула и поиск
  primary, повтор идемпотентных операций (чтение, `with_tenant_keyed`) с нарастающей паузой до `database.failover`.
- Неидемпотентные записи без ключа — не повторять, ошибка `Unavailable` с `retry_after`.
- Тест при `TEST_DATABASE_URL`: смена адреса primary между вызовами — чтение повторено и успешно.
