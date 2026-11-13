# CTR-83: признак «скрыта из книги «Организация»» у учётной записи без LDAP

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/meta.rs, docs/CONTRACTS.md]
read: [docs/tasks/META-65.md, docs/tasks/AUTH-16.md]
checks: [nextest]
manual: no

## Сделать
- Без LDAP книгу строит `Directory` поверх PostgreSQL (META-65, #681), но у учётной записи в PostgreSQL нет признака
  `hidden_from_gal` — скрыть можно только группу (отчёт meta-41). С LDAP признак берётся из `msExchHideFromAddressLists`
  (AUTH-16).
- Признак в `Account`/`AccountUpsert`, реализации (`tp-meta` с миграцией — номер у оркестратора, `tp-testkit`), Admin API
  `PATCH` учётной записи и консоль (UI-15) — отдельными волнами после контракта.

## Приёмка
- Тест набора: учётная запись с признаком не попадает в `Directory::accounts_since`.
