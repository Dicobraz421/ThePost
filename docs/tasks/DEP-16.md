# DEP-16: эталоны задают кодировку UTF8 и LC_CTYPE UTF-8 базы

lane: deploy
deps: []
class: A
files: [deploy/ansible/roles/postgresql/*, deploy/compose/dev.yml, deploy/compose/postgres/*, deploy/helm/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

## Сделать
- Ansible: база `thepost` создаётся с `encoding: UTF8`, `lc_ctype`/`lc_collate` `C.UTF-8` (`template0`); на уже
  созданной базе с другой настройкой — понятная ошибка, не пересоздание.
- Compose и Helm: `POSTGRES_INITDB_ARGS` / аналог — `--encoding=UTF8 --locale=C.UTF-8`, если образ по умолчанию иной.

## Приёмка
- На чистой установке `SELECT datctype FROM pg_database WHERE datname = 'thepost'` — UTF-8.
