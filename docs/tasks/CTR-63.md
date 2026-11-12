# CTR-63: `AccountStore::by_ids` — учётные записи по списку идентификаторов

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md#Metadata]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 49: admin-13, PR #487. имена участников группы читаются по одной записи — медленно на больших организациях.

## Сделать
- `AccountStore::by_ids(tenant, &[AccountId]) -> Vec<Account>` (не найденные пропускаются, порядок — как в запросе);
  по умолчанию — `get` в цикле. testkit, набор, `docs/CONTRACTS.md`, CHANGELOG. Реализация — META-66.

## Приёмка
- CI зелёный; тест набора: три идентификатора, один несуществующий — две записи по порядку.
