# CMP-09: метрики индексатора и doctor для служебных доменов

lane: app
deps: [CMP-06]
class: A
files: [crates/thepost/src/*]
read: []
checks: [nextest]
manual: no

Отчёты search-03 (CMP-06) и deploy-08 (CMP-01).

## Сделать
- `/metrics`: счётчики `Indexer::stats()` (`failures`, `retrying`) — регистрация в `crates/thepost/src/metrics.rs`.
- `protocols.rs`: повторяющаяся ошибка — `warn` не чаще раза за паузу (как индексатор).
- `doctor`: проверки DNS (`dns.mx` и др.) для доменов зарезервированных зон (`.test`, `.example`, `.invalid`, `.localhost`,
  RFC 6761) — `SKIP` с пояснением, а не `FAIL` (эталон `--profile smb` использует `example.test`).
