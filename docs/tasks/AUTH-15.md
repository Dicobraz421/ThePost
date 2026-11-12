# AUTH-15: синхронизация групп LDAP: участники по `external_id`

lane: auth
deps: [CTR-32]
class: A
files: [crates/tp-auth/src/*]
read: [docs/tasks/CTR-32.md]
checks: [nextest]
manual: no

Хвост волны contracts-20 (CTR-32), PR #286: синхронизация групп по-прежнему сопоставляет участников-учётные записи по имени
входа (`ldap/sync.rs`), приёмка CTR-32 не выполнена.

## Сделать
- `ldap/sync.rs`: участник-учётная запись — по `AccountStore::by_external_id` (идентификатор каталога), имя входа — только
  для записей без `external_id`.

## Приёмка
- Тест (приёмка CTR-32): переименование входа в каталоге не теряет членство в группе.
