# APP-50: узел: сессии Admin API в PostgreSQL и их очистка

lane: app
deps: [META-64, ADM-28]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 77 (этап 3).

## Сделать
- Роль `admin`: `AdminSessions` из `tp-meta`; очистка истёкших сессий — шаг роли maintenance (как APP-39).

## Приёмка
- Тест узла: вход на одном узле, запрос с той же сессией — на другом; истёкшие сессии удаляются maintenance.
