# SMTP-17: запись журнала доставки

lane: smtp
deps: [SMTP-03, SMTP-09, META-22]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

## Сделать
- События приёма, отказа (с причиной), доставки, передачи наружу и DSN в `DeliveryLog`; без содержимого писем.

## Приёмка
- Тест сквозного пути на фейках: все события записаны.
