# APP-05: Сквозной тест MVP

lane: app
deps: [APP-04]
class: A
files: [crates/thepost/tests/e2e.rs]
read: [docs/spec/APP-D.md#Д.7]
checks: [nextest]
manual: no

## Сделать
- SMTP-приём → IMAP FETCH и JMAP Email/get → поиск → удаление в корзину → восстановление.

## Приёмка
- Тест проходит в CI с PostgreSQL.
