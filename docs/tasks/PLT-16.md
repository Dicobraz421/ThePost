# PLT-16: минимальные привилегии при запуске

lane: platform
deps: [SYS-07, PLT-06]
class: A
files: [crates/tp-platform/src/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S2.md#2.7]
checks: [nextest]
manual: no

## Сделать
- Отказ запуска от root без явной настройки; приём сокетов systemd (`LISTEN_FDS`) вместо bind портов < 1024.
- Применение профилей SYS-07 после инициализации ролей; `thepost doctor` — проверка учётной записи, capabilities, seccomp, Landlock.

## Приёмка
- Тест передачи сокетов через переменные systemd; doctor сообщает о запуске от root.
