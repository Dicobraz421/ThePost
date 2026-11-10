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

## Состояние (cluster-06)
- Сделано (`crates/tp-cluster/src/rpc.rs`): маршрут `route` по `live_info()` и rendezvous (`Rendezvous::ranked_of`),
  предел параллельности `Admission` (`Busy`), матрица разрешений `Permissions` (`<роль>/<имя>`, `<роль>/*`; по умолчанию —
  запрещено всё), секция `RpcConfig` (`max_in_flight`, `connect_timeout`, `permissions`).
- Не сделано: реализация `Rpc` (HTTPS/2 с mTLS, сроки, отказ владельца — следующий, тесты на двух экземплярах) — у крейта
  нет зависимостей `axum`, `reqwest`, `bytes`, `tokio-rustls`/`hyper`, `x509-parser` (имя узла из сертификата вызывающего);
  ждёт PR `x/cluster-dep`.
