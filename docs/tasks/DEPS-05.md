# DEPS-05: rust-embed — веб-консоль внутри бинарного файла

lane: deps
deps: [CI-06]
class: B
files: [Cargo.toml, Cargo.lock, crates/tp-admin/Cargo.toml]
read: [docs/spec/S5.md#5.2, docs/decisions/2026-11-01-scale-shards-geo-admin.md]
checks: [nextest]
manual: no

## Сделать
- `rust-embed` (MIT) в workspace и в tp-admin за фичей `console` (каталог `console/dist`); без фичи и без каталога — сборка проходит,
  консоль отвечает страницей «консоль не собрана».
- `cargo deny` (лицензии, дубликаты).

## Приёмка
- `cargo deny`, `cargo audit` зелёные; tp-admin собирается с фичей `console` и без неё.
