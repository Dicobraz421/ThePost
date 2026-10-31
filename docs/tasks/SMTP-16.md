# SMTP-16: проверки до 250: квота, группы рассылки

lane: smtp
deps: [SMTP-13, MS-12, META-21]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S4.md#4.1, docs/spec/S4.md#4.5]
checks: [nextest]
manual: no

## Сделать
- RCPT TO: квота получателя (4xx или 5xx — настройка), ограничения отправителей группы; раскрытие группы, постановка письма в очередь модерации.

## Приёмка
- Тесты: переполненный ящик отклонён до DATA; внешний отправитель в закрытую группу отклонён; модерация.
