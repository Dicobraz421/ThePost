# SMTP-22: правила потока почты

lane: smtp
deps: [CTR-23, SMTP-05, SMTP-16, SMTP-17]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-01-transport-scope-external-mta.md, docs/spec/S4.md#4.1, docs/spec/S4.md#4.5, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `MailFlow`: условия и исключения, действия, порядок по `priority`, `stop`, режим `Audit` — только запись в журнал доставки.
- Вызов в smtp-in после Milter и в submission до очереди; один раз на письмо (письмо из submission внутреннему адресату правилами повторно не обрабатывается).
- Изменённое тело (дисклеймер, тема, заголовки) — новый объект BLOB вне транзакции; снимок правил — по сигналу `TransportChanged`.

## Приёмка
- Тесты на каждое действие и условие; `Audit` не меняет письмо; повторного применения нет; дисклеймер в text/plain и text/html.
