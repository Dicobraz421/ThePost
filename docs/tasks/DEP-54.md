# DEP-54: развёртывание: принципал `pop/<узел>` в keytab для входа GSSAPI по POP3

lane: deploy
deps: [DEP-39]
class: A
files: [deploy/README.md]
read: [docs/tasks/DEP-39.md, crates/tp-imap/src/pop3.rs]
checks: [none]
manual: no

## Сделать
- POP3 объявляет SASL GSSAPI со службой `pop` (`tp-imap/src/pop3.rs`, `SaslContext { service: "pop" }`, RFC 5034),
  а `deploy/README.md` перечисляет только `smtp/`, `imap/`, `sieve/<узел>`.
- `deploy/README.md`, «Вход по Kerberos (GSSAPI)»: POP3 в списке протоколов, принципал `pop/<узел>` рядом с `smtp/`,
  `imap/`, `sieve/`; примеры `kadmin` и SPN Active Directory — с `pop/`; `klist` — четыре принципала.

## Приёмка
- Список принципалов совпадает со службами `SaslContext` в коде (`smtp`, `imap`, `pop`, `sieve`).
