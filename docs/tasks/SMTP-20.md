# SMTP-20: доверенные источники: шлюз и приложения

lane: smtp
deps: [CTR-24, SMTP-12, SMTP-04]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-01-transport-scope-external-mta.md, docs/spec/S4.md#4.1, docs/spec/S2.md#2.6]
checks: [nextest]
manual: no

## Сделать
- Сопоставление клиента с `TrustedSource` по адресу или сертификату mTLS при соединении.
- Шлюз: XCLIENT и XFORWARD (только от шлюза, иначе 550); SPF, DMARC, экран, лимиты — по исходному клиенту; без данных — результаты шлюза по `authserv_id` и ARC.
- Приложение: отправка без SASL, только разрешённые отправители, внешние получатели — при `external`.

## Приёмка
- Тесты: XFORWARD от чужого адреса отклонён; SPF по исходному IP; приложение с чужим отправителем — 550, внешний получатель без разрешения — 550.
