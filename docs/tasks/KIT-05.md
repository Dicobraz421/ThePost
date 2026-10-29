# KIT-05: Фейки Ф1: SASL, Authenticator, поиск, очередь, корзина

lane: testkit
deps: [CTR-06, CTR-07, CTR-08, CTR-09]
class: A
files: [crates/tp-testkit/src/fakes.rs]
read: [docs/CONTRACTS.md#Auth, docs/CONTRACTS.md#Search, docs/CONTRACTS.md#Queue (Ф1), docs/CONTRACTS.md#Trash, Retention, Quota (Ф1)]
checks: [nextest]
manual: no

## Сделать
- FakeAuthenticator (таблица логин → пароль/токен), FakeSasl (PLAIN, OAUTHBEARER), FakeSearch (поиск подстроки по корпусу),
  MemQueue, MemTrash, MemQuota.

## Приёмка
- Протокольные полосы (smtp, imap, jmap, admin) тестируются без PostgreSQL и сети.
