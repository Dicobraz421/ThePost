# APP-24: thepost doctor — кодировка и LC_CTYPE базы

lane: app
deps: []
class: A
files: [crates/thepost/src/doctor.rs]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Проверка СУБД в `doctor`: `pg_database.encoding` = UTF8 и `datctype` — UTF-8 (`*.UTF-8`/`*.utf8`); `C`/`POSIX` или
  не-UTF8 — `FAIL` с пояснением (кириллица в именах входа не переводится в нижний регистр) и ссылкой на
  `docs/PREREQUISITES.md`.

## Приёмка
- Юнит-тест разбора значений; в CI на PostgreSQL — `OK`.
