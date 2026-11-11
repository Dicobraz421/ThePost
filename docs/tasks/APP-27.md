# APP-27: ManageSieve и JMAP календари, контакты, Sieve в узле

lane: app
deps: [SIEVE-09, JMAP-29, APP-25, APP-26]
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S4.md#4.3, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

## Сделать
- Слушатель ManageSieve (`sieve.managesieve.listen`, порт 4190) в роли `imap`, вход — `Guarded`.
- JMAP: `DavStore` и `tp_sieve::check` в `tp_jmap::Deps`; capabilities контактов, календарей и Sieve (JMAP-24) в сессии.

## Приёмка
- Интеграционный тест узла: `LISTSCRIPTS` по ManageSieve, `CalendarEvent/set` по JMAP виден в CalDAV.
