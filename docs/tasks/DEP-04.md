# DEP-04: Стенд SMB и базовая нагрузка

lane: deploy
deps: [APP-05]
class: A
files: [deploy/stand/*]
read: [docs/spec/APP-D.md#Д.2, docs/spec/APP-D.md#Д.3, docs/spec/APP-D.md#Д.5, docs/spec/APP-D.md#Д.6]
checks: [none]
manual: yes

## Сделать
- Ansible: ВМ SMB + генератор; сценарии imaptest, smtp-source, k6 по модели Д.3; пороги стенда Д.5; сбор отчёта.

## Приёмка
- Базовый сценарий запускается одной командой; ручная проверка — docs/manual-tests/DEP-04.md.
