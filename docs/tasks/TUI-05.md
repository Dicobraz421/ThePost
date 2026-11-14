# TUI-05: подкоманда `thepost admin tui` в узле

lane: app
deps: [TUI-01]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#118]
checks: [nextest]
manual: no

## Сделать
- Подкоманда `thepost admin tui` запускает `tp-tui` с теми же ключами подключения, что `thepost admin` (адрес, CA,
  `--insecure`, токен из `THEPOST_ADMIN_TOKEN`).

## Приёмка
- Тест разбора аргументов; `thepost admin tui --help`.
