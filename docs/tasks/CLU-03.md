# CLU-03: Шина событий Valkey

lane: cluster
deps: [CLU-01]
class: A
files: [crates/tp-cluster/src/valkey_bus.rs]
read: [docs/CONTRACTS.md#EventBus]
checks: [nextest]
manual: no

## Сделать
- Pub/sub по каналам tenant/account, переподключение, метрики; интеграционный тест при TEST_VALKEY_URL.

## Приёмка
- Тест совместимости поведения с CLU-01 (общий набор тестов).
