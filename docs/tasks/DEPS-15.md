# DEPS-15: зависимости картинок и мыши текстового интерфейса — `ratatui-image`, `evdev`

lane: deps
deps: [DEPS-14]
class: B
files: [Cargo.toml, Cargo.lock, crates/tp-tui/Cargo.toml]
read: [docs/decisions/2026-11-11-stage2-followups.md#119]
checks: [deny]
manual: no

## Сделать
- `ratatui-image` (протоколы kitty graphics, sixel, iTerm2, полублоки; версия, совместимая с `ratatui` 0.30) — курсор
  мыши картинкой и знак IT TASK на экране входа.
- `evdev` (только `target_os = "linux"`) — пиксельные координаты мыши из `/dev/input` на консоли Linux (уровень 3
  курсора); `ioctl` `KDFONTOP` — через уже имеющийся в дереве `libc` или `rustix`.
- `cargo deny` проходит (лицензии, дубликаты).

## Приёмка
- `cargo check -p tp-tui` (Linux и Windows) и `cargo deny` зелёные; сборка выпуска (`release-build`) не сломана.
