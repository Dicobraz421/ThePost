# DEP-17: эталон — UTC в PostgreSQL, chrony, текст ошибки «база не в UTF-8»

lane: deploy
deps: [DEP-16]
class: A
files: [deploy/ansible/roles/postgresql/*, deploy/ansible/roles/chrony/*, deploy/ansible/smb.yml, deploy/ansible/pg-ha.yml, docs/PREREQUISITES.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/PREREQUISITES.md]
checks: [none]
manual: no

## Сделать
- Роль postgresql: `timezone = 'UTC'`, `log_timezone = 'UTC'` (решение 24).
- Роль `chrony` (пакет дистрибутива): установка, включение, серверы — `thepost_ntp_servers` (по умолчанию пул
  дистрибутива); подключить в `smb.yml` и `pg-ha.yml` до остальных ролей (решение 25).
- Ошибка на базе не в UTF-8 — «пересоздайте базу в UTF-8» (решение 29); раздел в `docs/PREREQUISITES.md`: только чистые
  установки, как пересоздать базу.

## Приёмка
- На стенде: `SHOW timezone` — `UTC`; `chronyc tracking` — синхронизировано; повторный прогон — без изменений.
