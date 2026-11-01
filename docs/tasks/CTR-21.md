# CTR-21: контракт AppPasswordStore (пароли приложений)

lane: contracts
deps: [CTR-19, AUTH-02]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md, crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-testkit/src/*]
read: [docs/CONTRACTS.md#Auth]
checks: [nextest]
manual: no

## Сделать
- Трейт `AppPasswordStore` и `AppPassword` в tp-contracts вместо временного трейта `tp-auth` (AUTH-02, PR #121): `list`, `insert`, `revoke`; хранится только хэш.
- Реализация для `PgMeta` (миграция `0009_app_passwords`) и in-memory `MemAppPasswords` в tp-testkit — в том же PR (ветка `x/contracts-10`).
- Переход `tp-auth` на контракт и удаление его временной реализации `#[cfg(test)]` — волна полосы auth.

## Приёмка
- Общий набор `tp_testkit::suite::app_password_store` проходит на in-memory и на PostgreSQL.
