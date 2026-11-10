# APP-19: команда `thepost admin init` и локальный сокет

lane: app
deps: [ADM-11]
class: A
files: [crates/thepost/src/*]
read: []
checks: [nextest]
manual: docs/manual-tests/APP-19.md

Решение владельца #193.

## Сделать
- Подкоманда CLI `thepost admin init [--email] [--force]`: подключается к локальному сокету узла, выводит токен один раз.
- Локальный Unix-сокет узла (путь из конфигурации, права 0600 root) — до наложения Landlock.
- Ручная проверка: эталон `--profile smb` — `docker compose exec thepost thepost admin init`, вход в Postman.
