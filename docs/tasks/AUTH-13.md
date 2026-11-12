# AUTH-13: служба SASL `sieve` для ManageSieve

lane: auth
deps: [SIEVE-08]
class: A
files: [crates/tp-auth/src/*]
read: [docs/spec/S4.md#4.3, docs/CONTRACTS.md#Auth]
checks: [nextest]
manual: no

Хвост волны sieve-03, PR #441.

Требует подтверждения владельца: имя службы SASL для ManageSieve — `sieve` по RFC 5804 (рекомендация агента), а не `imap`? (PR #441, решение 2)

## Сделать
- ManageSieve входит с `SaslContext { service: "sieve", .. }` (RFC 5804). Проверить и закрепить тестами, что `tp-auth`
  пропускает это имя: `PLAIN` и `OAUTHBEARER` (области токена по имени службы не отсекают вход), GSSAPI —
  акцептор `sieve@<узел>` (`gssapi.rs`), билет `imap/…` для службы `sieve` не принимается.
- Если имя службы где-то сверяется со списком — добавить `sieve`; в описании секции `auth` — строка про принципал
  `sieve/<узел>` в keytab.

## Приёмка
- Тесты `sasl.rs`, `gssapi.rs`: вход `PLAIN` и `OAUTHBEARER` со службой `sieve`; акцептор запрошен для `sieve`.
