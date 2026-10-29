# AUTH-04: SASL-механизмы

lane: auth
deps: [CTR-08, AUTH-02, AUTH-03]
class: A
files: [crates/tp-auth/src/sasl.rs]
read: [docs/spec/S5.md#5.1]
checks: [nextest]
manual: no

## Сделать
- PLAIN, LOGIN, OAUTHBEARER (RFC 7628), XOAUTH2 поверх Authenticator.

## Приёмка
- Тесты обменов по примерам RFC.
