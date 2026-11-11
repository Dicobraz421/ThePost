# DEPS-01: обновить calamine и libsqlite3-sys, сократить пропуски cargo audit

lane: deps
deps: [CI-03]
class: B
files: [Cargo.toml, Cargo.lock, deny.toml, .github/workflows/ci.yml]
read: [docs/decisions/2026-11-11-contracts-ci-defaults.md]
checks: [nextest]
manual: no

Решение владельца 2026-11-11, п. 3.

## Сделать
- calamine → ≥ 0.38, libsqlite3-sys → ≥ 0.39 (через крейт, который его тянет); поправить вызовы, если API изменился.
- Убрать их записи из списка `--ignore` `cargo audit` (job `deny`) и из `deny.toml`, если там есть.
- zip < 9 (через mail-auth) — оставить в пропусках, в комментарии — ссылка на ожидаемый выпуск mail-auth.

## Приёмка
- `cargo audit` в CI зелёный без пропусков calamine и libsqlite3-sys; весь `nextest` зелёный.
