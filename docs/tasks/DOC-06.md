# DOC-06: INSTALL: `admin transport domain` вместо `admin domain`, вход `admin login --oidc`

lane: docs
deps: [ADM-38, ADM-39]
class: A
files: [docs/INSTALL.md]
read: [docs/tasks/ADM-38.md, docs/tasks/ADM-39.md]
checks: [none]
manual: no

Хвост admin-25 (#728).

## Сделать
- `docs/INSTALL.md`: команда `thepost admin domain …` удалена (ADM-38) — пример и описание на
  `thepost admin transport domain list|create|delete`.
- `docs/INSTALL.md`: `thepost admin login --oidc` подключена (ADM-39) — убрать оговорку «ключи `cli.oidc` пока только
  проверяются», коротко описать вход: код в stderr, токен в stdout → `THEPOST_ADMIN_TOKEN`.
- `docs/releases/0.2.0.md` описывает выпущенную 0.2.0 — не трогать.

## Приёмка
- Команды и флаги совпадают с `thepost admin --help` (`crates/thepost/src/admin_client.rs`).
