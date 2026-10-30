# DEP-05: Эталонная установка SMB одной командой

lane: deploy
deps: [DEP-02]
class: A
files: [deploy/ansible/*]
read: [docs/spec/S1.md#1.2, docs/spec/S5.md#5.6]
checks: [none]
manual: no

## Сделать
- Ansible-роли: PostgreSQL, Rspamd (+ clamav-milter опционально), бинарник, systemd, config, ACME; запуск одной командой; идемпотентно.

## Приёмка
- Чистая ВМ → рабочий SMB одной командой; повторный прогон без изменений.
