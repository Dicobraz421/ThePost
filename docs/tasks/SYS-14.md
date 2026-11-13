# SYS-14: `fs_usage` на `nix::sys::statvfs`

lane: sys
deps: [DEPS-13]
class: A
files: [crates/tp-sys/src/*]
read: [docs/tasks/SYS-13.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 63: sys-08, PR #520. Готовое вместо своего.

## Сделать
- `fs_usage` — через `nix::sys::statvfs::statvfs`, свой `unsafe` удалить.

## Приёмка
- Тесты SYS-13 зелёные.
