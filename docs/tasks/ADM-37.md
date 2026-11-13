# ADM-37: CLI `account list --after`

lane: admin
deps: [APP-22]
class: A
files: [crates/thepost/src/admin_client.rs]
read: [docs/tasks/APP-41.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 65: app-39, PR #530. Часть APP-41 в `admin_client.rs` — полоса admin (решение 87).

## Сделать
- `AccountAction::List`, `execute`: ключ `--after`, разбор `Link rel="next"`, подсказка следующей страницы (APP-41).

## Приёмка
- Тест CLI против роутера: две страницы через `--after` без пропусков и повторов.
