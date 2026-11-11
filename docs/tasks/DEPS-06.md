# DEPS-06: HTTP/2 для межузловых вызовов — фичи axum и reqwest

lane: deps
deps: [CLU-14]
class: B
files: [Cargo.toml, Cargo.lock]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Фичи `axum/http2`, `reqwest/http2` в workspace (решение 31); `cargo deny` и размер выпуска — без регрессий.

## Приёмка
- CI зелёный; `cargo tree -e features` показывает h2.
