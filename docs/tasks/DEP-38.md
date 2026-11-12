# DEP-38: ManageSieve в поставке: секция и порт 4190

lane: deploy
deps: [APP-27]
class: A
files: [deploy/*]
read: [docs/tasks/APP-27.md, docs/tasks/SIEVE-08.md]
checks: [none]
manual: no

Хвост реестра облачных волн, п. 54: app-26, PR #491. после установки из поставки ManageSieve выключен: в `deploy/` нет секции
`sieve.managesieve.listen` и порта 4190.

## Сделать
- `deploy/config/*.yaml` (smb, all, mailbox, access): `sieve.managesieve.listen` на 4190 (STARTTLS); роль `thepost` и
  межсетевой экран — порт 4190; сокет systemd `thepost-managesieve.socket`; Helm — порт сервиса; `deploy/README.md`.

## Приёмка
- `thepost config check`, `--syntax-check`, `helm template` без ошибок; на стенде `sieve-connect` к 4190 входит паролем
  приложения.
