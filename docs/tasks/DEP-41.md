# DEP-41: restore.yml на семействе RHEL — обёртки полным путём

lane: deploy
deps: [DEP-23]
class: A
files: [deploy/ansible/restore.yml]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

## Сделать
- На RHEL-семействе `sudo` не видит `/usr/local/sbin`: в `restore.yml` вызывать служебные обёртки полным путём, как
  уже сделано в ролях (отчёт deploy-16, #476).

## Приёмка
- `restore.yml` на AlmaLinux 9 в контейнере доходит до восстановления без «command not found».
