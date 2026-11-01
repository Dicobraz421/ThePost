# AUTH-08: блокировки перебора на всех узлах

lane: auth
deps: [META-27]
class: A
files: [crates/tp-auth/src/*]
read: [docs/decisions/2026-11-01-interaction-db-ha.md, docs/spec/S6.md#6.3]
checks: [nextest]
manual: no

## Сделать
- Счётчики попыток — на узле; при превышении порога — `AbuseBlocks::block`; проверка блокировки — локальный набор, обновляемый по сигналу `Blocked` и при `Resync`.

## Приёмка
- Тест на двух экземплярах testkit: блокировка на одном узле действует на другом.
