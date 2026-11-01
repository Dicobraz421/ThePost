# APP-09: сборка настроек транспорта и точки правил потока

lane: app
deps: [META-28, SMTP-22, JMAP-10, ADM-08]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/decisions/2026-11-01-transport-scope-external-mta.md, docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- Подключить `TransportStore` (PostgreSQL) в smtp-in, smtp-out, submission, admin; `MailFlow` из `Registry` (модуль) — в smtp-in, submission, jmap; перечитывание снимка по `TransportChanged` и `Resync`.

## Приёмка
- Интеграционный тест: доверенный источник, созданный через Admin API, действует на следующую сессию без перезапуска; тестовая реализация `MailFlow` через `Registry` вызывается один раз на письмо.
