# DEP-45: эталон: только встроенный ACME, certbot удалить

lane: deploy
deps: [PLT-29]
class: A
files: [deploy/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 9; решение 101.

## Сделать
- Удалить `tls-certbot.yml`, режим `thepost_tls_mode: certbot`, примеры инвентаря, стенд (`stand_generator`) и
  `deploy/README.md`; режим `acme` — по умолчанию для публичного имени.

## Приёмка
- `--syntax-check` и `ansible-lint` без ошибок; `grep -r certbot deploy` пуст.
