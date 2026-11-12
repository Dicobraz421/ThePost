# CTR-74: `Outbound`: учётная запись отправителя при постановке

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 75: smtp-07, PR #548. `Outbound::schedule` и `enqueue` передают `owner = None` — при нескольких шардах отложенное письмо
может лечь не на шард ящика.

## Сделать
- `Outbound::schedule`/`enqueue` принимают учётную запись отправителя (решение 46); места вызова — в этом же PR.

## Приёмка
- CI зелёный.
