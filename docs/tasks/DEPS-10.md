# DEPS-10: `tower-http` (CORS) для `tp-admin`

lane: deps
deps: [DEPS-05]
class: B
files: [Cargo.toml, Cargo.lock, crates/tp-admin/Cargo.toml]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

Синк 12.11, часть 2, решение 68: CORS в Admin API — готовым слоем (решение 2026-10-31 «готовое вместо своего»).

## Сделать
- `tower-http` (MIT) в workspace с фичей `cors`, в `tp-admin` — зависимостью; версия — совместимая с `axum` workspace.
- `cargo deny check` и `cargo audit` без новых пропусков; число новых пакетов — в PR.

## Приёмка
- CI зелёный; `cargo tree -p tp-admin -i tower-http` показывает одну версию.
