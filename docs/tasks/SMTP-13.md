# SMTP-13: адресация: catch-all, подадреса, переписывание

lane: smtp
deps: [SMTP-03, AUTH-01]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- Подадреса `user+tag` (разделитель — настройка, доставка в ящик user, тег — для Sieve), catch-all домена, переписывание адресов отправителя и получателя по таблице.

## Приёмка
- Тесты разрешения адресов на фейках testkit.
