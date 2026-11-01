# SMTP-22: точки вызова правил потока почты

lane: smtp
deps: [CTR-24, SMTP-05, SMTP-16, SMTP-17]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/decisions/2026-11-01-transport-scope-external-mta.md, docs/spec/S4.md#4.1, docs/spec/S4.md#4.5, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Вызов `MailFlow` (если модуль зарегистрирован) в smtp-in после Milter и в submission до очереди; один раз на письмо (письмо из submission внутреннему адресату повторно не обрабатывается).
- Исполнение исхода: `Reject` — 5xx с текстом, `Drop`, `Moderate` — очередь модерации (п. 4.5), изменённые конверт и тело, `route` — политика домена назначения; `matched` — в журнал доставки.

## Приёмка
- Тесты на заглушке `MailFlow`: каждый исход; без модуля вызова нет; повторного применения нет.
