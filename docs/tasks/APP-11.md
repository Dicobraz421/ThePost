# APP-11: CLI thepost admin

lane: app
deps: [ADM-01]
class: A
files: [crates/thepost/src/*, crates/thepost/build.rs, crates/thepost/openapi.json]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

## Сделать
- Подкоманда `thepost admin <ресурс> <действие>`: клиент — reqwest, каждая команда сверяется тестом со спецификацией tp-admin (progenitor не читает OpenAPI 3.1 — решение `2026-11-11-stage2-followups.md`, п. 13; типы — APP-21); зависимость — PR `x/app-dep`.
- Вход: токен OIDC (device flow или из переменной окружения), адрес — из конфигурации или ключа; вывод — таблица или JSON (`--json`).

## Приёмка
- Тесты: команды списка и создания ящика против тестового Admin API; расхождение `openapi.json` с маршрутами — ошибка теста.
