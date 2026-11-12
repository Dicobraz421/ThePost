# ADM-22: CLI — удаление скрипта, автоответ и скрипт организации

lane: admin
deps: [ADM-17, CI-07]
class: A
files: [crates/thepost/src/admin_client.rs]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

Хвост волны admin-12 (CLI — ветка `x/admin-sieve-cli`), PR #440; команды автоответа — хвост волны app-25, PR #442.

Требует подтверждения владельца: команды автоответа `thepost admin vacation` — отдельной волной после admin-12 (вариант Б агента), в полосе admin после CI-07? (PR #442, решение 1)

## Сделать
- `thepost admin sieve delete <учётная запись> <имя>` — `DELETE …/sieve/scripts/{name}`.
- Автоответ: `thepost admin vacation get|set` — `GET`/`PUT …/accounts/{account}/vacation` (`set --off` выключает).
- Скрипт организации: `thepost admin sieve org get|put|delete` — `GET`/`PUT`/`DELETE /api/v1/orgs/{org}/sieve`;
  `put --file <путь>` (`-` — stdin), ошибки текста (422) — с позициями, как у `sieve put`; в справке `sieve org put` —
  скрипты организации и пользователя выполняются подряд, запрещающее правило организации пишется `discard; stop;`
  (решение 76).
- Операции добавить в `OPERATIONS` (сверка с OpenAPI); `--json`, `--org` — как у остальных команд.

## Приёмка
- Тест `sieve_commands_against_admin_api` дополнен: удаление скрипта, включение и выключение автоответа, запись и
  удаление скрипта организации; `parses_admin_commands` — разбор новых команд.
