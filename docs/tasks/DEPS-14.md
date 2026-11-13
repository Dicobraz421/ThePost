# DEPS-14: зависимости текстового интерфейса — `ratatui`, `crossterm`

lane: deps
deps: []
class: B
files: [Cargo.toml, Cargo.lock, crates/tp-tui/Cargo.toml]
read: [docs/decisions/2026-11-11-stage2-followups.md#118]
checks: [deny]
manual: no

## Сделать
- Новый крейт `crates/tp-tui` в workspace (манифест и `lib.rs` с модулем, который наполнит TUI-01); зависимости
  `ratatui`, `crossterm` (актуальные стабильные); `cargo deny` проходит.

## Приёмка
- `cargo check -p tp-tui` и `cargo deny` зелёные; сборка выпуска (`release-build`) не сломана.
