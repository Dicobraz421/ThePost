# CLU-09: удалить шину Valkey

lane: cluster
deps: [CTR-23]
class: A
files: [crates/tp-cluster/*, docs/DEV.md]
read: [docs/decisions/2026-11-01-interaction-db-ha.md]
checks: [nextest]
manual: no

## Сделать
- Удалить `valkey_bus.rs`, конфигурацию и метрики Valkey; зависимость `redis` — отдельным PR `x/cluster-dep`.
- Убрать `TEST_VALKEY_URL` из DEV.md.

## Приёмка
- Сборка и тесты полосы без Valkey; `cargo tree` без `redis`.
