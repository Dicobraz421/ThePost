# META-45: побайтный порядок списка учётных записей — индекс COLLATE "C"

lane: meta
deps: [CTR-44]
class: B
files: [crates/tp-meta/migrations/0019_accounts_login_c.sql, crates/tp-meta/src/accounts.rs, crates/tp-meta/tests/pg_accounts.rs, crates/tp-testkit/src/suite.rs]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Миграция `0019`: индекс `(tenant_id, (lower(login) COLLATE "C"))` (`CREATE INDEX` — без блокировки записи, если
  миграции это позволяют; иначе обычный — таблица SMB мала).
- `PgMeta::list`: `ORDER BY lower(login) COLLATE "C"`, курсор `lower(login) COLLATE "C" > $after`, префикс —
  диапазоном `>= $prefix AND < $prefix_next` по тому же индексу вместо `starts_with`.
- Набор `suite::account_list` дополнить именами, различающимися знаками `.`, `-`, `_` и не-ASCII: порядок PostgreSQL
  и памяти совпадает.

## Приёмка
- `suite::account_list` проходит в памяти и на PostgreSQL с новыми именами; план запроса использует новый индекс
  (`EXPLAIN` в тесте или в отчёте).
