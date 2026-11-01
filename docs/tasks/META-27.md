# META-27: состав кластера с ролями и блокировки

lane: meta
deps: [CTR-23, META-16]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/decisions/2026-11-01-interaction-db-ha.md, docs/spec/S2.md#2.8, docs/spec/S6.md#6.3, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Записи узлов: роли, адрес служебного канала и площадка (`site`), `live_info()`.
- `AbuseBlocks` в PostgreSQL (по организации, RLS), после записи — сигнал `Blocked`.

## Приёмка
- Тесты миграции и conformance testkit.
