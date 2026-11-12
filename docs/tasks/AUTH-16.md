# AUTH-16: LDAP: атрибут «скрыта из GAL» у учётной записи

lane: auth
deps: [CTR-58]
class: A
files: [crates/tp-auth/src/*]
read: [docs/CONTRACTS.md#Directory]
checks: [nextest]
manual: no

Хвост волны dav-03 (DAV-12), PR #443, решение 1.

## Сделать
- Соединитель LDAP: атрибут скрытия (настройка синхронизации, по умолчанию `msExchHideFromAddressLists`) →
  `DirAccount::hidden_from_gal`.

## Приёмка
- Тест на in-memory каталоге: запись с атрибутом — `hidden_from_gal = true`.
