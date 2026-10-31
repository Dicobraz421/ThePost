# SMTP-15: политики по доменам назначения

lane: smtp
deps: [SMTP-09, SMTP-10, CTR-23]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/decisions/2026-11-01-transport-scope-external-mta.md, docs/spec/S4.md#4.1, docs/spec/S6.md#6.2]
checks: [nextest]
manual: no

## Сделать
- Политика домена: обязательный TLS, закреплённый сертификат или CA, smart host, egress-IP; приоритет над MTA-STS и DANE — строже из двух.
- Политики — из `TenantTransport::routes` (`pattern` = `*` — все внешние домены), перечитывание по сигналу `TransportChanged`.

## Приёмка
- Тесты: отказ без TLS для домена с обязательным TLS; маршрут через smart host.
