# APP-09: сборка настроек транспорта и правил потока

lane: app
deps: [META-28, SMTP-22, JMAP-10, ADM-08]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-01-transport-scope-external-mta.md, docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- Подключить `TransportStore` (PostgreSQL), `MailFlow` (tp-smtp) в smtp-in, submission, jmap, admin; перечитывание снимка по `TransportChanged` и `Resync`.

## Приёмка
- Интеграционный тест: правило, созданное через Admin API, применяется к следующему письму без перезапуска.
