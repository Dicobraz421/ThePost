# PLT-27: `PoolMeter` — реализация наблюдателя ожидания соединения

lane: platform
deps: [CTR-51, PLT-25]
class: A
files: [crates/tp-platform/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост волны platform-15, PR #434.

## Сделать
- `tp_platform::dbmetrics::PoolMeter` реализует `AcquireObserver` (CTR-51): те же `tp_db_pool_waiting`,
  `tp_db_pool_wait_seconds`, `tp_db_pool_acquire_errors_total`; `PoolMeter::acquire` остаётся и выражается через трейт.
- `DbMetrics` отдаёт `PoolMeter` пула как `Arc<dyn AcquireObserver>` — его узел передаёт в `tp-meta` (APP-34).

## Приёмка
- Тест PLT-25 «при исчерпании пула растёт время ожидания» проходит через трейт; отмена не оставляет «ожидает».
