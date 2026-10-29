# AUTH-02: Пароли

lane: auth
deps: [CTR-05]
class: A
files: [crates/tp-auth/src/password.rs]
read: [docs/spec/S5.md#5.1]
checks: [nextest]
manual: no

## Сделать
- Проверка пароля LDAP bind; пароли приложений (argon2) с отзывом; защита от перебора через PLT-07 — вызовом из протоколов.

## Приёмка
- Тесты: верный/неверный пароль, отозванный пароль приложения.
