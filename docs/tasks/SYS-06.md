# SYS-06: ресурсы узла по cgroups с перечитыванием

lane: sys
deps: [SYS-02]
class: A
files: [crates/tp-sys/src/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S2.md#2.7]
checks: [nextest]
manual: no

## Сделать
- Лимиты из cgroup v2 (`memory.max`, `cpu.max`, `memory.current`, `cpu.stat`), отслеживание изменений (inotify/опрос с интервалом из секции).
- Канал `watch` с текущими лимитами и использованием.

## Приёмка
- Тест на подменном каталоге cgroups: изменение `memory.max` доходит до подписчика.
