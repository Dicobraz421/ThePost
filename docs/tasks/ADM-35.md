# ADM-35: CLI: проверка скрипта Sieve без `tp_sieve::check`

lane: admin
deps: [JMAP-35]
class: A
files: [crates/thepost/src/admin_client.rs, crates/tp-admin/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/tasks/JMAP-35.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 3; решение 95.

## Сделать
- `admin_client.rs`: локальную предпроверку `tp_sieve::check` удалить — текст проверяет узел с пределами секции `sieve`
  (ошибки 422 с позицией выводятся как есть); комментарии и тестовая подстановка `tp-admin` — без ссылок на `check`.

## Приёмка
- Тест CLI против роутера: неверный скрипт — ошибка узла с позицией.
