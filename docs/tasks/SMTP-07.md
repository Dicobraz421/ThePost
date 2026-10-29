# SMTP-07: DSN

lane: smtp
deps: [SMTP-06]
class: A
files: [crates/tp-smtp/src/dsn.rs]
read: [docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- Уведомления о недоставке и задержке (RFC 3464) через mail-builder, доставка отправителю.

## Приёмка
- Снимки DSN; формат проверяется mail-parser.
