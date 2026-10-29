# SMTP-01: SMTP-сервер: сессия

lane: smtp
deps: [PLT-06]
class: A
files: [crates/tp-smtp/src/server/*]
read: [docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- Конечный автомат сессии (smtp-proto): EHLO, MAIL, RCPT, DATA, BDAT, RSET, PIPELINING, SIZE, 8BITMIME, SMTPUTF8, ENHANCEDSTATUSCODES; лимиты.

## Приёмка
- Тесты сценариев сессии по RFC 5321.
