# SMTP-35: адрес клиента в AUTH SMTP

lane: smtp
deps: [CTR-57]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/spec/S6.md#6.3]
checks: [nextest]
manual: no

Хвост волны app-16, PR #293: протоколы не передают адрес клиента в проверку учётных данных.

## Сделать
- Submission и smtp-in: `SaslContext.peer` — адрес клиента соединения (после PROXY protocol, если он включён).

## Приёмка
- Тест: AUTH PLAIN — в проверку учётных данных пришёл адрес клиента сессии.
