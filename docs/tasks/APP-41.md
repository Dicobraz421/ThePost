# APP-41: CLI `thepost admin account list --after`

lane: app
deps: [APP-22]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост волны app-21 (APP-22), PR #378: продолжение списка с произвольного имени входа (`--after`) в CLI не добавлено.

## Сделать
- `account list --after <имя входа>` — параметр `after` Admin API (ADM-12); несовместим с `--login`, как `--prefix`;
  при выводе страницы — подсказка с `--after` для следующей (из ссылки `rel="next"`).

## Приёмка
- Тест CLI против роутера tp-admin: две страницы через `--after` без пропусков и повторов; `--after` с `--login` — ошибка.
