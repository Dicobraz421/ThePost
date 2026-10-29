# ADM-04: Пароли приложений

lane: admin
deps: [ADM-01, AUTH-02]
class: A
files: [crates/tp-admin/src/app_passwords.rs]
read: [docs/spec/S5.md#5.1]
checks: [nextest]
manual: no

## Сделать
- Выпуск (показ один раз), список, отзыв.

## Приёмка
- Тест: отозванный пароль не проходит AUTH-02.
