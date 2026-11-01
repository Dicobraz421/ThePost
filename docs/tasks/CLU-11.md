# CLU-11: запросы узел → узел (Rpc)

lane: cluster
deps: [CTR-23]
class: A
files: [crates/tp-cluster/src/*]
read: [docs/decisions/2026-11-01-interaction-db-ha.md, docs/spec/S2.md#2.8, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `Rpc` поверх HTTPS/2 с mTLS (axum + reqwest, `InternalChannel::Node`); маршрут по `live_info()` и rendezvous-хэшированию, при отказе — следующий.
- Ограничение параллельности (`Busy`), сроки (`Timeout`), матрица разрешений «роль → службы» по сертификату вызывающего.
- Параметры — в секции конфигурации.

## Приёмка
- Тесты на двух экземплярах в процессе: маршрут, отказ владельца, `Busy`, отказ по матрице.
