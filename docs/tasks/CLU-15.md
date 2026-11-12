# CLU-15: Rpc — h2 в ALPN

lane: cluster
deps: [DEPS-06]
class: A
files: [crates/tp-cluster/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- ALPN `h2`, `http/1.1`; клиент — одно соединение HTTP/2 на узел (решение 31).

## Приёмка
- Тест: два узла договариваются о `h2`, параллельные вызовы идут по одному соединению.
