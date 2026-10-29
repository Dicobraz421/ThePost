# IMAP-02: Сессия и аутентификация

lane: imap
deps: [IMAP-01, PLT-06, AUTH-04, KIT-05]
class: A
files: [crates/tp-imap/src/session.rs]
read: [docs/spec/S4.md#4.2, docs/spec/S5.md#5.1]
checks: [nextest]
manual: no

## Сделать
- Состояния, CAPABILITY, STARTTLS, LOGIN, AUTHENTICATE (SASL), LOGOUT, ENABLE, ID; drain с BYE.

## Приёмка
- Тесты сценариев на testkit.
