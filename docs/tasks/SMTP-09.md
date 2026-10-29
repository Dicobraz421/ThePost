# SMTP-09: Доставка наружу

lane: smtp
deps: [SMTP-06]
class: A
files: [crates/tp-smtp/src/outbound.rs]
read: [docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- MX-резолвинг с приоритетами, mail-send, оппортунистический STARTTLS, привязка к egress-IP, коды ответов → статусы очереди.

## Приёмка
- Тест с локальным SMTP-приёмником.
