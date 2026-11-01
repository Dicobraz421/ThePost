# ADM-08: Admin API настроек транспорта

lane: admin
deps: [ADM-01, CTR-24]
class: A
files: [crates/tp-admin/src/*]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/decisions/2026-11-01-transport-scope-external-mta.md, docs/spec/S4.md#4.1, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Маршруты: принятые домены и их тип, доверенные источники, политики доменов назначения; запись с `version`. Правила потока — Admin API модуля.

## Приёмка
- Тесты маршрутов на `MemTransport`; устаревшая версия — 409.
