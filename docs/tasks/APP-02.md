# APP-02: Команда doctor

lane: app
deps: [APP-01, META-07]
class: A
files: [crates/thepost/src/doctor.rs]
read: [docs/spec/S5.md#5.5]
checks: [nextest]
manual: no

## Сделать
- Агрегатор проверок полос: СУБД, BLOB (запись/чтение/удаление пробного объекта), время; вывод таблицей и JSON.

## Приёмка
- Код возврата ≠0 при критичной ошибке.
