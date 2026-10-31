# CTR-19: контракт AclStore (права и делегирование)

lane: contracts
deps: [CTR-05, META-12]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md, crates/tp-meta/src/*, crates/tp-testkit/src/*]
read: [docs/CONTRACTS.md#Auth]
checks: [nextest]
manual: no

## Сделать
- Трейт `AclStore` в tp-contracts поверх того, что уже умеет `PgMeta` (ACL ящиков и делегирования, META-12, PR #120); реализация для `PgMeta` и in-memory в tp-testkit — в том же PR (ветка `x/contracts-08`).

## Приёмка
- Общий набор `tp_testkit::suite::acl_store` проходит на in-memory и на PostgreSQL.
