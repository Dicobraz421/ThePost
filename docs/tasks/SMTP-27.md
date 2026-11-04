# SMTP-27: профиль Milter для исходящей почты

lane: smtp
deps: [SMTP-05]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-04-exchange-parity.md, docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- Отдельный профиль Milter для submission и JMAP EmailSubmission (политика при недоступности — своя).

## Приёмка
- Тесты на заглушке Milter: отказ и изменение письма при отправке.
