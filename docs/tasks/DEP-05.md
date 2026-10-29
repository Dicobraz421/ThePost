# DEP-05: Установка SMB

lane: deploy
deps: [DEP-02]
class: A
files: [deploy/ansible/*]
read: [docs/spec/S1.md#1.2]
checks: [none]
manual: no

## Сделать
- Ansible-роль: PostgreSQL, бинарник, systemd, config, ACME; идемпотентна.

## Приёмка
- Повторный прогон без изменений.
