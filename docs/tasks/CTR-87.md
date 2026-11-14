# CTR-87: `Directory::contacts_since` без тела по умолчанию

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/auth.rs, docs/CONTRACTS.md]
read: [docs/tasks/META-80.md]
checks: [nextest]
manual: no

## Сделать
- После META-80 (#745) почтовые контакты есть у всех реализаций `Directory` (LDAP, PostgreSQL, `tp-testkit`); тело по
  умолчанию «каталог без контактов» прячет реализацию, забывшую контакты.
- Снять тело по умолчанию у `contacts_since`; реализации без своего метода, если найдутся, — в этом же PR.

## Приёмка
- `cargo check --workspace --all-targets` без новых заглушек; тесты набора каталога проходят.
