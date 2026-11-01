# APP-11: CLI thepost admin

lane: app
deps: [ADM-01]
class: A
files: [crates/thepost/src/*, crates/thepost/build.rs, crates/thepost/openapi.json]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

## Сделать
- Подкоманда `thepost admin <ресурс> <действие>`: клиент генерируется из OpenAPI Admin API (progenitor, сборка по `openapi.json`, который выгружает тест tp-admin); зависимость — PR `x/app-dep`.
- Вход: токен OIDC (device flow или из переменной окружения), адрес — из конфигурации или ключа; вывод — таблица или JSON (`--json`).

## Приёмка
- Тесты: команды списка и создания ящика против тестового Admin API; расхождение `openapi.json` с маршрутами — ошибка теста.
