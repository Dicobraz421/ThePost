# PLT-08: ACME-клиент (SMB)

lane: platform
deps: [PLT-06]
class: A
files: [crates/tp-platform/src/acme.rs]
read: [docs/spec/S6.md#6.2]
checks: [nextest]
manual: yes

## Сделать
- instant-acme, HTTP-01 на служебном HTTP, хранение сертификата и ключа на диске, продление за 30 дней.

## Приёмка
- Тест с моком ACME-каталога (без сети); ручная проверка — docs/manual-tests/PLT-08.md.
