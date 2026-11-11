# META-43: локальные токены Admin API в PostgreSQL, подключение `admin init`

lane: meta
deps: [CTR-41, ADM-11, APP-19]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*, crates/thepost/src/local_admin.rs, docs/manual-tests/APP-19.md]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: docs/manual-tests/APP-19.md

Блокер этапа 1 «почта работает на стенде»: `thepost admin init` на эталоне отвечает `503`, потому что у
`tp_contracts::admin_token::LocalAdminTokens` (CTR-41, решение #193) нет реализации в PostgreSQL; in-memory на узле
недопустим (токен — вход администратора инсталляции). Межполосное (meta + app) — класс B.

## Сделать
- Миграция `0018_local_admin_tokens.sql`: таблица `local_admin_tokens` (идентификатор, организация, учётная запись,
  hex SHA-256 значения, выпуск, срок, погашен, отозван); идентификатор и хэш уникальны по инсталляции; RLS по
  `tenant_id`; индексы под `consume` (хэш), `list` (организация, выпуск) и `prune` (срок).
- `impl LocalAdminTokens for PgMeta`: `issue`, `list` — в транзакции организации; `consume`, `revoke`, `prune` — по
  всем организациям (Bearer не несёт организацию). `consume` — один `UPDATE … WHERE … RETURNING`: из параллельных
  успешен ровно один.
- `thepost`: `local_admin::attach` подключает `with_local_tokens(PgMeta)`; убрать `TODO(META)` и оговорку про `503`
  в `docs/manual-tests/APP-19.md`.

## Приёмка
- Общий набор `tp_testkit::suite::local_admin_tokens` на `PgMeta` при `TEST_DATABASE_URL`.
- Гонка: параллельные `consume` одного токена — успешен ровно один, остальные `Forbidden`.
- Ручная проверка — `docs/manual-tests/APP-19.md`: `admin init` выдаёт токен, вход по нему — один раз.
