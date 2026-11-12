# DEP-40: эталон на ВМ Astra Linux SE 1.8

lane: deploy
deps: [DEP-19]
class: A
files: [deploy/ansible/*, docs/manual-tests/DEP-40.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/manual-tests/DEP-19.md, docs/PREREQUISITES.md]
checks: [none]
manual: yes

## Сделать
- На ВМ Astra Linux SE 1.8 с её ядром: `smb.yml` — `failed=0`, повтор — `changed=0`, `thepost doctor` без `FAIL`,
  письмо между двумя пользователями; режимы МРД и МКЦ (PARSEC) — работа служб и песочницы Landlock/seccomp; ЗПС —
  что нужно подписать заказчику (`thepost`, OpenBao, PGDG) — по итогам строкой в `docs/PREREQUISITES.md` (решение 89).
- В контейнере не проверить: systemd без модуля PARSEC не запускается (отчёт deploy-16, #476).

## Приёмка
- `docs/manual-tests/DEP-40.md` с итогом по каждому режиму.
