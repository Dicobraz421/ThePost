# CLU-14: Rpc — идентичность узла по SAN, HTTP/2

lane: cluster
deps: [CLU-12]
class: A
files: [crates/tp-cluster/src/*]
read: []
checks: [nextest]
manual: no

Отчёт cluster-07 (CLU-12): узел опознаётся по CN сертификата, ALPN — только `http/1.1`.

## Сделать
- Идентичность узла — по SAN (URI `thepost:node:<id>` или DNS-имя из `NodeInfo.addr`), CN — только если SAN нет
  (совместимость с выпуском CLU-04); тест с сертификатом без CN.
- HTTP/2 после включения фич `axum/http2`, `reqwest/http2` (PR `x/cluster-dep-2`): `h2` в `ALPN`.
- Перечитывание сертификатов по событию (как PLT-17/PLT-19), без перезапуска.
