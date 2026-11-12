# CTR-66: журнал доставки: событие iTIP

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md#Delivery]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 32: smtp-13, PR #447. вида события для приёма iTIP в журнале доставки нет — пишется только в `tracing`.

## Сделать
- Вид события журнала доставки `Itip { method, result }` (применено, пропущено с причиной). CHANGELOG.

## Приёмка
- CI зелёный.
