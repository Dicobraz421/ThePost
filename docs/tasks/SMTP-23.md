# SMTP-23: SRS и ARC-подпись при пересылке

lane: smtp
deps: [SMTP-08, SMTP-13]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-01-transport-scope-external-mta.md, docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- Пересылка (redirect Sieve, перенаправление правилом): адрес отправителя в конверте — SRS0/SRS1 с ключом из хранилища секретов; ARC-подпись (mail-auth).
- Приём DSN на адрес SRS — обратное преобразование и проверка подписи и срока.

## Приёмка
- Тесты: SRS туда и обратно, просроченный или подделанный адрес — 550; ARC-подпись проверяется mail-auth.
