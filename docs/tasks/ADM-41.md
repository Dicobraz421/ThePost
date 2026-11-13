# ADM-41: вход администратора по паролю

lane: admin
deps: []
class: B
files: [crates/tp-admin/src/*, crates/thepost/src/admin_client.rs, crates/thepost/src/local_admin.rs,
  crates/thepost/Cargo.toml, Cargo.toml, Cargo.lock, console/src/*, console/README.md, README.md,
  deploy/stand/roles/stand_generator/files/provision-accounts.sh, docs/INSTALL.md, docs/manual-tests/DEP-12.md,
  crates/tp-contracts/src/meta.rs, crates/tp-meta/src/accounts.rs, crates/tp-meta/src/schema.rs,
  crates/tp-meta/migrations/0040_installation_admins.sql, crates/tp-meta/tests/pg_accounts.rs,
  crates/tp-testkit/src/meta.rs, crates/tp-testkit/src/suite.rs, crates/tp-imap/src/tests/ext.rs, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [fmt, clippy, nextest, console]
manual: no

## Сделать
Решение 117 (владелец 14.11: «По паролю администратора»). Сейчас без OIDC сессию Admin API даёт только обмен токена
`thepost admin init`; истекла сессия — войти можно лишь `admin init --force`, который сбрасывает пароль.
- Admin API: `POST /api/v1/session` с телом `{login, password}` — проверка пароля `Deps::auth` (путь входа
  пользователя узла, `AbuseGuard` по адресу клиента), роли — `admin.roles` и запись о роли администратора установки.
  Без ролей — 403, неверный пароль — 401, `admin.password_login` выключен — 404.
- Роль администратора установки — явная запись (решение владельца 14.11 «Переделать роль»): `AccountStore::
  grant_installation_admin`/`is_installation_admin` без реализации по умолчанию — `PgMeta` (таблица
  `installation_admins`, миграция 0040), `MemMetadata`, набор `suite::account_installation_admin`. `admin init` ставит
  запись, вход по паролю читает её; распознавания по подписи пароля приложения и резерва подписи (400) нет. Миграция
  переносит роль учётной записи `admin init`, заведённой раньше (действующий пароль с подписью `thepost admin init` в
  организации по умолчанию), данные не удаляет. Каждая попытка — в журнал аудита. `GET /api/v1/session` —
  поле `password`. Схема `openapi_spec()` и копия консоли (CI-11) — в том же PR.
- CLI: `thepost admin login --user <login>` — пароль из TTY без эха (`rpassword`) или `--password-stdin`, не из
  аргумента; обмен токена `admin init` в CLI и подсказки «новый — `admin init --force`» удаляются.
- Консоль: форма логина и пароля, когда вход по паролю включён; форма токена `admin init` — только когда выключен.
- Стенд (`provision-accounts.sh`): сессия — вход по паролю `admin init` из состояния стенда, без `init --force`.
- `docs/INSTALL.md`: вход по паролю, `admin.password_login`.

## Приёмка
- Тесты tp-admin: верный пароль — сессия; неверный — 401 и запись в аудит; без роли — 403; выключено — 404 без
  проверки пароля; адрес клиента передаётся проверке; после `admin init` вход по паролю — `installation_admin`;
  пароль приложения с подписью `thepost admin init`, выпущенный Admin API, роли не даёт.
- Тесты tp-meta (CI): набор `account_installation_admin`; миграция 0040 на базе схемы 0039 переносит роль учётной
  записи `admin init` (отозванный пароль, другая подпись и другая организация — нет), пароли остаются.
- Тест thepost: перебор пароля с одного адреса блокирует вход по паролю и с верным паролем (`Guarded`); CLI
  `admin login --password-stdin` — сессия.
