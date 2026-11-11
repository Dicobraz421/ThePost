# DEP-19: эталон Ansible на Astra Linux SE 1.8

lane: deploy
deps: [DEP-15, DEP-17]
class: A
files: [deploy/ansible/*, docs/PREREQUISITES.md, docs/INSTALL.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/PREREQUISITES.md]
checks: [none]
manual: yes

## Сделать
- Роли (postgresql, openbao, rspamd, clamav_milter, thepost, pgbackrest, chrony) работают на Astra Linux SE 1.8:
  репозитории и имена пакетов, версии PostgreSQL дистрибутива, совместимость Landlock/seccomp с мандатным контролем
  (PARSEC) — проверить, ограничения записать (решение 28).
- Совместимость выпуска: glibc дистрибутива ≥ требуемой бинарным файлом.
- Строки в `docs/PREREQUISITES.md` и `docs/INSTALL.md`.

## Приёмка
- `smb.yml` на чистой Astra Linux SE 1.8 — `failed=0`, `thepost doctor` без `FAIL`, письмо между двумя пользователями —
  `docs/manual-tests/DEP-19.md`.
