# APP-34: метрики Д.7 в `/metrics` узла — пул СУБД, слушатель, NOTIFY, «сироты»

lane: app
deps: [PLT-25, PLT-27, META-53, META-55, BLOB-16]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/spec/APP-D.md#Д.7]
checks: [nextest]
manual: no

Хвост волн platform-15 (PR #434), meta-34 (PR #433), blob-09 (PR #432).

Подтверждено владельцем — решение 70 (`docs/decisions/2026-11-11-stage2-followups.md`).

## Сделать
- Пул и слушатель (PR #434): `DbMetrics::register`; `db.pool("primary", || PoolSample { size, idle, max })` для пула
  узла и пулов шардов (метка `pool`); `PoolMeter` передаётся в `tp-meta` как `AcquireObserver` (PLT-27, META-55);
  `db.listener(...)` — из `PgSignalBus::connected()`, `subscribers()`, `metrics()`.
- NOTIFY (PR #433): `infra.rs::pg_bus_sample` и регистрация читают новые поля `SignalMetrics` — гистограммы
  `notify_commit` и `commit` (`Histogram::cumulative()` → корзины `le`), `listeners`, `notify_commit_share()`;
  при шардах — `ShardedSignalBus::metrics()`. Имена — в ряду `tp_db_*`, перечислить в PR для отчёта DEP-13.
- «Сироты» (PR #432): в `metrics.rs` — `register(metrics, &orphans, OrphanSweeper::metrics, OrphanMetrics::STATS)` для
  воркера из `roles.rs` (`tp_blob_orphans_{passes,found,deleted}_total`, `tp_blob_orphans_remaining`).

## Приёмка
- Тест узла: вывод `/metrics` содержит `tp_db_pool_in_use`, `tp_db_pool_wait_seconds`, `tp_db_listener_connected`,
  метрики времени фиксаций с NOTIFY и числа слушателей, `tp_blob_orphans_remaining`.
- Строки Д.7 про пул, NOTIFY и «сирот» заполняются по этим метрикам на стенде (`deploy-21`).
