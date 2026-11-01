# CTR-32: поиск учётной записи по external_id

lane: contracts
deps: [CTR-05]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md, crates/tp-meta/src/*, crates/tp-testkit/src/*]
read: []
checks: [nextest]
manual: no

## Сделать
- `AccountStore::by_external_id` (идентификатор из каталога LDAP); синхронизация групп AUTH-07 сопоставляет участников по нему, а не по имени входа (PR #168). Реализации в tp-meta и tp-testkit — в том же PR (ветка `x/contracts-20`).

## Приёмка
- Тест: переименование входа в каталоге не теряет членство в группе.
