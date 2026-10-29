# CLU-01: Шина событий в процессе

lane: cluster
deps: [CTR-04]
class: A
files: [crates/tp-cluster/src/local_bus.rs]
read: [docs/CONTRACTS.md#EventBus]
checks: [nextest]
manual: no

## Сделать
- broadcast по учётке с ограниченным буфером; отставший подписчик получает Lagged и пересинхронизируется.

## Приёмка
- Тесты доставки и переполнения.
