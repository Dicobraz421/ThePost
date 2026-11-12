# DEP-27: роль thepost: не ждать готовности узла; копия OpenBao в deploy/README

lane: deploy
deps: [DEP-23]
class: A
files: [deploy/ansible/roles/thepost/*, deploy/README.md]
read: [docs/manual-tests/DEP-23.md, docs/INSTALL.md]
checks: [none]
manual: no

Хвост волны deploy-19 (DEP-23), PR #421: на новом сервере первый `smb.yml` падает на `Wait for thepost readiness`, если в
настройках есть DKIM (ключ вернётся только из снимка OpenBao); снимок OpenBao не описан в `deploy/README.md`.

## Сделать
- Роль `thepost`: переключатель `thepost_wait_ready` (по умолчанию `true`); `false` — плейбук не ждёт `/healthz/ready`
  и завершается успешно с предупреждением (сценарий «новый сервер перед `restore.yml`»). Умолчание владельца из синка
  12.11 — вариант «б»; инструкция INSTALL — DOC-02.
- `deploy/README.md`, раздел «Резервное копирование и восстановление»: снимок Raft OpenBao (`thepost_backup_openbao`,
  `thepost-backup-openbao.service`, AppRole `thepost-backup-snapshot`), восстановление `-e thepost_restore_openbao=true`
  с ключом распечатывания, `chown` каталога копий на новой ВМ.

## Приёмка
- `ansible-lint` и `--syntax-check` без ошибок; на стенде `smb.yml -e thepost_wait_ready=false` на новой ВМ с DKIM —
  `failed=0`.
