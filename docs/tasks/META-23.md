# META-23: раздельные учётные записи PostgreSQL

lane: meta
deps: [META-13, META-17]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S3.md#3.1, docs/spec/S2.md#2.7]
checks: [nextest]
manual: no

## Сделать
- Владелец схемы — только для миграций (`thepost migrate`), узлы — роли с минимальными правами: общая роль приложения и роль только чтения метаданных для search и indexer.
- Учётные данные ролей — ссылками на секреты; проверка в `thepost doctor`: роль узла не владелец схемы, нет BYPASSRLS и SUPERUSER.

## Приёмка
- Тесты: роль search не изменяет метаданные писем; миграции от роли приложения отклоняются.
