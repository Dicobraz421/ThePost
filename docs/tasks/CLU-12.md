# CLU-12: транспорт Rpc узел → узел (HTTPS/2, mTLS)

lane: cluster
deps: [CLU-11]
class: A
files: [crates/tp-cluster/src/*]
read: [docs/spec/S2.md#2.8]
checks: [nextest]
manual: no

Отчёт cluster-06 (CLU-11): маршрут, предел параллельности и матрица разрешений есть, транспорта нет. Зависимости
(axum, reqwest, bytes, tokio-rustls/hyper, x509-parser; убрать redis, rustls) — PR `x/cluster-dep`.

## Сделать
- `Rpc` поверх HTTPS/2 с mTLS внутреннего канала (идентичность узла — из сертификата), сроки (`Timeout`), `Unavailable` при
  недоступном узле; `RpcHandler` на стороне сервера.
- Тест на двух экземплярах в процессе: вызов, отказ по матрице, `Busy`, `Timeout`.
