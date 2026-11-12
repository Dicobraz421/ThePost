# AUTH-14: SASL передаёт адрес клиента в проверку учётных данных

lane: auth
deps: [CTR-57]
class: A
files: [crates/tp-auth/src/*]
read: [docs/spec/S6.md#6.3, docs/CONTRACTS.md#SASL]
checks: [nextest]
manual: no

Хвост волны app-16, PR #293 (защита от перебора считает только учётную запись).

## Сделать
- `AuthSasl` (PLAIN, LOGIN, OAUTHBEARER, XOAUTH2, GSSAPI) вызывает `verify_from(credentials, ctx.peer)` вместо `verify`.
- `AbuseGuard` уже считает по `Attempt::ip` и подсети (`ip_failures`, `net_prefix_v4/v6`) — проверить, что неудачи с
  несуществующими именами входа засчитываются адресу.

## Приёмка
- Тест: SASL PLAIN с адресом в контексте — фейк проверки получил тот же адрес; неудачи несуществующих имён с одного
  адреса блокируют адрес.
