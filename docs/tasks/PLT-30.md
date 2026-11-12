# PLT-30: метрика брошенных ожиданий соединения с СУБД

lane: platform
deps: [PLT-27]
class: A
files: [crates/tp-platform/src/*]
read: [docs/tasks/PLT-27.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 53: platform-17, PR #489. счётчик `tp_db_pool_acquire_cancelled_total` не заведён — массовые отмены при
перегрузке базы видны только косвенно.

## Сделать
- `PoolMeter`: брошенное ожидание соединения (future отменён) — `tp_db_pool_acquire_cancelled_total{pool}`, рядом с
  гистограммой ожидания.

## Приёмка
- Тест: отменённое ожидание увеличивает счётчик и не попадает в гистограмму.
