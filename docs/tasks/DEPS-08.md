# DEPS-08: `serde_yaml_ng` в dev-зависимостях `tp-sieve`

lane: deps
deps: [SIEVE-07]
class: B
files: [crates/tp-sieve/Cargo.toml, Cargo.lock]
read: [docs/decisions/2026-10-31-ready-made-first.md]
checks: [nextest]
manual: no

Хвост волны sieve-02, PR #426.

## Сделать
- `crates/tp-sieve/Cargo.toml`, `[dev-dependencies]`: `serde_yaml_ng.workspace = true` (уже есть в workspace) — для
  теста чтения секции `sieve` из YAML (SIEVE-10). В бинарный файл не попадает.

## Приёмка
- CI зелёный; `cargo deny` без новых пакетов и дубликатов.
