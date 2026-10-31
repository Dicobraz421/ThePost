# CLU-08: убрать ValkeyBusConfig и ValkeyBus::start

lane: cluster
deps: [CLU-07, APP-07]
class: A
files: [crates/tp-cluster/src/*]
read: []
checks: [nextest]
manual: no

## Сделать
- Удалить оставленные ради `thepost` до APP-07 (PR #99) `ValkeyBusConfig` (поля `*_ms`) и `ValkeyBus::start`; остаётся `ValkeyConfig` и `ValkeyBus::connect`.

## Приёмка
- В tp-cluster нет старых типов; `thepost` собирается без правок.
