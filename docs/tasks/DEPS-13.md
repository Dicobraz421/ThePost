# DEPS-13: `nix` с признаком `fs` для `tp-sys`

lane: deps
deps: [SYS-13]
class: B
files: [Cargo.toml, Cargo.lock, crates/tp-sys/Cargo.toml]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 63: sys-08, PR #520. `fs_usage` написан на `unsafe statvfs`, чтобы не включать `fs` у `nix`.

## Сделать
- Признак `fs` у `nix` в `tp-sys` (новых пакетов нет).

## Приёмка
- `cargo deny` без новых записей.
