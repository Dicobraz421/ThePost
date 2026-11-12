# ADM-31: CLI: блокировка и изменение учётной записи

lane: admin
deps: [ADM-18, CI-07]
class: A
files: [crates/thepost/src/admin_client.rs]
read: [docs/tasks/ADM-18.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 48: admin-13, PR #487. команды CLI для `PATCH …/accounts/{account}` нет.

## Сделать
- `thepost admin account set <login|id> [--disable|--enable] [--display-name <имя>]` — по шаблону CLI (полоса admin
  после CI-07, решение 87).

## Приёмка
- Тест `operations_match_openapi` и прогон против роутера tp-admin.
