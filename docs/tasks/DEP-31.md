# DEP-31: прогон pg-ha и «малого Enterprise» после правки community.postgresql

lane: deploy
deps: [DEP-17, DEP-18]
class: A
files: [deploy/ansible/*, docs/manual-tests/DEP-09.md, docs/manual-tests/DEP-10.md]
read: [docs/manual-tests/DEP-09.md, docs/manual-tests/DEP-10.md]
checks: [none]
manual: yes

Хвосты волн deploy-15 (PR #406: `pg-ha.yml` и `small-enterprise.yml` после замены параметров community.postgresql не
прогонялись) и deploy-12 (PR #412: `{{ ansible_managed }}` в `copy: content` — остался в `small-enterprise.yml`, keepalived).

## Сделать
- Прогон `pg-ha.yml` и `small-enterprise.yml` на стенде с community.postgresql 4.x и 5.x, повторный — без изменений;
  находки — правки ролей.
- `small-enterprise.yml`, «Write keepalived.conf»: `{{ ansible_managed }}` вне шаблона не определён — текст, как в роли
  `thepost` (DEP-15).

## Приёмка
- Итоги прогонов в `docs/manual-tests/DEP-09.md` и `DEP-10.md` без «не прошёл».
