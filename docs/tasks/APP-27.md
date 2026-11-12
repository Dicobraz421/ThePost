# APP-27: ManageSieve и JMAP календари, контакты, Sieve в узле

lane: app
deps: [SIEVE-09, JMAP-29, APP-29, APP-26, AUTH-13]
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S4.md#4.3, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

## Сделать
- Слушатель ManageSieve (`sieve.managesieve.listen`, порт 4190) в роли `imap`, вход — `Guarded`.
- Имя службы SASL у ManageSieve — `sieve` (RFC 5804; хвост волны sieve-03, PR #441): вход `PLAIN`, `OAUTHBEARER` и
  Kerberos (принципал `sieve/<узел>` в keytab) проверяется тестом узла; поддержка в `tp-auth` — AUTH-13.
- Секция `sieve.managesieve.listen` — список слушателей (`EndpointConfig`), пустой список — ManageSieve выключен.
- JMAP: `DavStore` и `tp_sieve::check` в `tp_jmap::Deps`; capabilities контактов, календарей и Sieve (JMAP-24) в сессии.

## Приёмка
- Интеграционный тест узла: `LISTSCRIPTS` по ManageSieve, `CalendarEvent/set` по JMAP виден в CalDAV.
