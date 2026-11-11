# SYS-12: Landlock — резолвер systemd-resolved (Ubuntu 24.04)

lane: sys
deps: []
class: A
files: [crates/tp-sys/src/confine/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: yes

## Сделать
- Проверить на Ubuntu 24.04 (контейнер или ВМ стенда), что узел с Landlock разрешает имена, когда `/etc/resolv.conf` —
  ссылка в `/run/systemd/resolve/`. Подтвердится — каталог в `system_read` по умолчанию (решение 26).

## Приёмка
- `thepost doctor` и разрешение имён на Ubuntu 24.04 с Landlock — без ошибок; итог — `docs/manual-tests/SYS-12.md`.
