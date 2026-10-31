# ADM-06: роли оператора и аудитора, экспорт аудита

lane: admin
deps: [ADM-01]
class: A
files: [crates/tp-admin/src/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S5.md#5.2, docs/spec/S6.md#6.8]
checks: [nextest]
manual: no

## Сделать
- Роли «только чтение» и «аудитор» (только журнал аудита); экспорт журнала аудита в syslog (RFC 5424, TLS) и OTLP.
- Admin API по умолчанию слушает только внутренние адреса.

## Приёмка
- Тесты прав ролей.
