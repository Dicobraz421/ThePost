# PLT-07: Rate limit

lane: platform
deps: [PLT-01]
class: A
files: [crates/tp-platform/src/ratelimit.rs]
read: [docs/spec/S6.md#6.3]
checks: [nextest]
manual: no

## Сделать
- Token bucket по IP, подсети /24 и /64, учётной записи; временная блокировка источника с TTL; лимиты из конфигурации.

## Приёмка
- Тесты на ManualClock: превышение блокирует, по TTL снимается.
