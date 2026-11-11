# CTR-44: AccountStore::list — постраничный список учётных записей

lane: contracts
deps: [ADM-12]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md, crates/tp-meta/src/*, crates/tp-meta/tests/*, crates/tp-admin/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md]
checks: [nextest]
manual: no

Решение владельца 2026-11-11, п. 14.

## Сделать
- `AccountStore::list(tenant, prefix, after, limit) -> Result<Vec<Account>>`: по возрастанию `lower(login)`,
  `lower(login)` начинается с `prefix` (нижний регистр; пусто — все) и строго больше `after`; не более `limit`.
  Метод по умолчанию — `Error::Internal` (как `by_external_id`).
- Реализации: `PgMeta` (курсор по индексу `accounts_login (tenant_id, lower(login))`), `MemMetadata`; общий набор testkit.
- tp-admin: список — через `accounts.list(..)`; временный трейт `AccountList` и `Admin::with_account_list` удалить.

## Приёмка
- Общий набор на in-memory и PostgreSQL: страницы, курсор, префикс, регистр, разделение организаций.
- На узле `GET /api/v1/orgs/{org}/accounts` без `login` отвечает списком, а не 503.
