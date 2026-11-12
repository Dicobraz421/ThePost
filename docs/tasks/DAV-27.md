# DAV-27: GAL: скрытые учётные записи не попадают в книгу «Организация»

lane: dav
deps: [CTR-58, DAV-12]
class: A
files: [crates/tp-dav/src/*]
read: [docs/tasks/DAV-12.md]
checks: [nextest]
manual: no

Хвост волны dav-03 (DAV-12), PR #443, решение 1.

## Сделать
- Фильтр построения GAL: учётные записи с `hidden_from_gal` исключаются (как скрытые группы).

## Приёмка
- Тест: скрытая запись не видна в `addressbook-query` и `sync-collection`, после снятия признака — появляется.
