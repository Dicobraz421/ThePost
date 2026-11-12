# DEPS-07: фича `console` в крейте `thepost`

lane: deps
deps: [DEPS-05]
class: B
files: [crates/thepost/Cargo.toml, Cargo.lock]
read: [docs/spec/S5.md#5.2, docs/decisions/2026-11-01-scale-shards-geo-admin.md]
checks: [nextest]
manual: no

Хвост волны deps-05, PR #431.

Подтверждено владельцем — решение 68 (`docs/decisions/2026-11-11-stage2-followups.md`).

## Сделать
- `crates/thepost/Cargo.toml`: `[features] console = ["tp-admin/console"]` — бинарный файл с консолью собирается
  `cargo build -p thepost --features console`; по умолчанию фича выключена, `Cargo.lock` без новых пакетов.
- Перевод сборок CI, беты и образа на эту фичу — CI-08.

## Приёмка
- `cargo check -p thepost` и `cargo check -p thepost --features console` (без `console/dist`) проходят; `cargo deny` зелёный.
