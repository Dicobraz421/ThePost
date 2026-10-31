# ADM-07: группы рассылки: модерация и настройки

lane: admin
deps: [ADM-01, META-21]
class: A
files: [crates/tp-admin/src/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S4.md#4.5]
checks: [nextest]
manual: no

## Сделать
- Маршруты: группы и их ограничения, очередь модерации (одобрить, отклонить с уведомлением), скрытие из GAL.

## Приёмка
- Тесты маршрутов на фейках.
