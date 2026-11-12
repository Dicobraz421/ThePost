# DEP-42: эталоны без `smtp.delivery.local_domains`

lane: deploy
deps: []
class: A
files: [deploy/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 4; решение 96.

## Сделать
- Compose (`thepost-smb.yaml`), роль `thepost` (умолчания, шаблон), `deploy/README.md` (строка про домен эталона):
  ключ удалить; домен эталона заводится принятым доменом организации в БД (`thepost admin transport domain create` при
  установке, как DEP-12 провижининг), если ещё не заводится.

## Приёмка
- Стенд: письмо между двумя учётными записями эталона доставляется без ключа в config.yaml.
