# SMTP-05: Milter-клиент

lane: smtp
deps: [SMTP-03]
class: A
files: [crates/tp-smtp/src/milter/*]
read: [docs/spec/S4.md#4.1]
checks: [nextest]
manual: yes

## Сделать
- Протокол Milter v6: цепочка фильтров, макросы, действия (accept, reject, tempfail, add header, quarantine), таймауты, политика при недоступности.

## Приёмка
- Тесты с мок-фильтром; ручная проверка с Rspamd — docs/manual-tests/SMTP-05.md.
