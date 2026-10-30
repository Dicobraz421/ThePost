# META-07: Пул соединений и проверки СУБД

lane: meta
deps: [META-01]
class: A
files: [crates/tp-meta/src/pool.rs, crates/tp-meta/src/doctor.rs]
read: [docs/spec/S3.md#3.1]
checks: [nextest]
manual: no

## Сделать
- Пул sqlx из конфигурации; режим совместимости с PgBouncer transaction; проверки для doctor: версия ≥16,
- synchronous_standby_names (Enterprise), max_connections, часовой пояс UTC;
- транзакция в контексте организации: `with_tenant(tenant, |tx| …)` выполняет SET LOCAL app.tenant_id.

## Приёмка
- Тест: проверки возвращают структурированный отчёт.
