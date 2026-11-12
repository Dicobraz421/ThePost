# DEP-43: эталоны: лимиты, селектор DKIM и Milter — политикой организации

lane: deploy
deps: [CTR-67]
class: A
files: [deploy/*]
read: [docs/tasks/CTR-67.md]
checks: [none]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 5; решение 97.

## Сделать
- Шаблон config.yaml роли `thepost`, compose, `deploy/config/*.yaml`: ключи лимитов и селектора удалить; значения
  эталона — политикой организации при установке (`thepost admin policy set`), `deploy/README.md`.

## Приёмка
- Стенд: подпись DKIM и лимит отправки действуют после установки.
