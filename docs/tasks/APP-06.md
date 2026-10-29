# APP-06: doctor: DNS и сертификаты

lane: app
deps: [APP-02]
class: A
files: [crates/thepost/src/doctor_dns.rs]
read: [docs/spec/S5.md#5.5]
checks: [nextest]
manual: no

## Сделать
- Проверки MX, SPF, DKIM, DMARC, MTA-STS, PTR, срок сертификатов, PROXY v2 на балансировщике.

## Приёмка
- Тесты с мок-резолвером.
