# DEPS-16: зависимости подсветки вывода CLI — `anstream`, `anstyle`, `toml`

lane: deps
deps: [ADM-44]
class: B
files: [Cargo.toml, Cargo.lock, crates/tp-admin-client/Cargo.toml]
read: [docs/decisions/2026-11-11-stage2-followups.md#120]
checks: [deny]
manual: no

## Сделать
- `anstream` и `anstyle` — прямыми зависимостями рабочего пространства (`[workspace.dependencies]`) в версиях, что
  уже в дереве `clap` (`anstream` 1.0, `anstyle` 1.0): второй копии крейтов в `Cargo.lock` нет.
- `toml` (с `serde`) — разбор раздела `[cli]` файла `design/tokens.toml` при сборке; версия — на тех же `toml_parser`
  и `toml_datetime`, что уже в дереве.
- `crates/tp-admin-client/Cargo.toml` — все три зависимости (`workspace = true`). Справка в цвете
  (`clap::builder::Styles`) — признак `color` у `clap`, он включён по умолчанию: правка `clap` не нужна.
- `cargo deny` проходит (лицензии, дубликаты).

## Приёмка
- `cargo check -p tp-admin-client -p thepost` (Linux и Windows) и `cargo deny` зелёные; в `Cargo.lock` по одной версии
  `anstream`, `anstyle`, `toml_parser`.
