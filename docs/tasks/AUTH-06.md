# AUTH-06: Авторизация и делегирование

lane: auth
deps: [CTR-05, AUTH-01, CTR-19]
class: A
files: [crates/tp-auth/src/authz.rs]
read: [docs/spec/S4.md#4.5]
checks: [nextest]
manual: no

## Сделать
- Authorizer: ACL RFC 4314, группы, общие ящики, Send As / Send on Behalf.

## Приёмка
- Матрица прав в тестах.
