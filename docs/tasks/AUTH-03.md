# AUTH-03: OIDC Bearer

lane: auth
deps: [CTR-05]
class: A
files: [crates/tp-auth/src/oidc.rs]
read: [docs/spec/S5.md#5.1]
checks: [nextest]
manual: no

## Сделать
- Discovery, кэш JWKS (moka), проверка подписи, aud, exp, маппинг claim → учётка.

## Приёмка
- Тесты с локальными ключами.
