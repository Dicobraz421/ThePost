# ADM-02: Организация, домены, учётки, квоты

lane: admin
deps: [ADM-01, MS-06]
class: A
files: [crates/tp-admin/src/accounts.rs]
read: [docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

## Сделать
- Организация по умолчанию (чтение, политики), CRUD доменов, учёток (поверх каталога — только локальные атрибуты), квоты.

## Приёмка
- Тесты API на testkit.
