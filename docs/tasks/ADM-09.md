# ADM-09: настройка trash.purge и восстановление скрытых писем

lane: admin
deps: [CTR-26, ADM-03]
class: A
files: [crates/tp-admin/src/*]
read: [docs/decisions/2026-11-01-trash-purge.md]
checks: [nextest]
manual: no

## Сделать
- Admin API: `trash.purge` и `user_can_choose` организации; список и восстановление скрытых писем ящика; запись в аудит.

## Приёмка
- Тест: администратор меняет настройку — пользователь без `user_can_choose` её не переопределяет; скрытое письмо восстанавливается.
