# ADM-02: Домены, учётки, квоты

lane: admin
deps: [ADM-01, MS-06]
class: A
files: [crates/tp-admin/src/accounts.rs]
read: [docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

## Сделать
- CRUD доменов, учёток (поверх LDAP-синхронизации — только локальные атрибуты), квоты.

## Приёмка
- Тесты API на testkit.
