# DEP-12: стенд SMB — учётные записи через thepost admin и наполнение по Д.3

lane: deploy
deps: [DEP-04, APP-21]
class: A
files: [deploy/stand/*, docs/manual-tests/DEP-12.md]
read: [docs/spec/APP-D.md#Д.3, docs/manual-tests/DEP-04.md, docs/INSTALL.md#учётные-записи]
checks: [none]
manual: yes

## Сделать
- `stand_provision: admin` (по умолчанию): `thepost admin init` на узле SMB (повторно — пропуск по `409`), `admin login`,
  заведение `user1…user<N>@<домен>` и пароля приложения каждому — идемпотентно (существующие пропускаются).
  Пароли стенда — в файле `0600` на генераторе, в журнал и вывод Ansible не попадают (`no_log`). LDIF остаётся вариантом.
- Наполнение до прогона (`--tags fill`): `stand_fill_messages` писем на ящик (Д.3: 1 500) из корпуса — параллельная
  доставка на 25 с ограничением скорости; один ящик `stand_big_mailbox` — 10 000 писем в одной папке (критерий
  «IMAP SELECT 10 тыс.»). Итог — число принятых и число видимых в ящиках (сверка по IMAP STATUS выборочно).
- Повторный запуск наполнения не дублирует: отметка выполненного объёма на генераторе.

## Приёмка
- `ansible-playbook … --tags provision,fill -e stand_users=50 -e stand_fill_messages=100` на уменьшенном стенде:
  50 учётных записей, вход IMAP каждой, 5 000 писем видны; повтор — без изменений.
