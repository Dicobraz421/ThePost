# ADM-32: темы скрытых писем и имена участников групп одним запросом

lane: admin
deps: [CTR-63, MS-19, ADM-19, ADM-20]
class: A
files: [crates/tp-admin/src/*]
read: [docs/tasks/ADM-19.md, docs/tasks/ADM-20.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 49: admin-13, PR #487.

## Сделать
- `GET …/trash/hidden?include=subject` — тема и отправитель из MS-19 с записью в аудит (как ADM-19).
- Имена участников группы (`GroupInfo.members`) — `AccountStore::by_ids` одним запросом вместо цикла.

## Приёмка
- Тесты: тема скрытого письма отдаётся только по `include=subject`, в аудите — запись; группа из 500 участников — один
  запрос к хранилищу.
