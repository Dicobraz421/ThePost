# PLT-03: Телеметрия

lane: platform
deps: []
class: A
files: [crates/tp-platform/src/telemetry.rs]
read: [docs/spec/S6.md#6.4]
checks: [nextest]
manual: no

## Сделать
- tracing JSON-логи, уровень из конфигурации; OTLP-экспорт логов и трейсов (опционально); реестр метрик prometheus-client.

## Приёмка
- Тест: метрика регистрируется и отдаётся в текстовом формате.
