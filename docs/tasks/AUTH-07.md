# AUTH-07: группы рассылки из каталога

lane: auth
deps: [AUTH-01, META-21]
class: A
files: [crates/tp-auth/src/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S4.md#4.5, docs/spec/S5.md#5.1]
checks: [nextest]
manual: no

## Сделать
- Инкрементальная синхронизация групп каталога (участники, вложенность, атрибуты ограничений и скрытия — сопоставление в секции) в `GroupStore`.

## Приёмка
- Тест на фейковом LDAP: вложенная группа, удаление участника.
