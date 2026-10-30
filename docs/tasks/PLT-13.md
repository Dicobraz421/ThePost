# PLT-13: длительности tp-platform — строки humantime

lane: platform
deps: [PLT-06, CTR-13]
class: A
files: [crates/tp-platform/src/*, docs/manual-tests/PLT-08.md]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Длительности в конфигурации (#43, вариант 2), docs/CONTRACTS.md#Config]
checks: [nextest]
manual: no

## Сделать
- Длительности секций tp-platform — строки humantime (`tp_contracts::config::duration`), поля без единицы в имени
  (решение #43): `tls.reload_interval`, `handshake_timeout` слушателя, `runtime.drain_deadline`, `runtime.rss_interval`,
  `ratelimit.block_duration`, `acme.renew_before`, `acme.check_interval`, `acme.retry_interval`, `acme.order_timeout`.
- `0s` там, где ранее `0` выключал механизм (`tls.reload_interval`, `runtime.rss_interval`, `ratelimit.block_duration`).

## Приёмка
- Тесты разбора секций с новыми именами; число без единицы — ошибка разбора.
