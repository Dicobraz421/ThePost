# CTR-73: очередь: ближайший срок попытки и сигнал пополнения

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 74: smtp-07, PR #548. smtp-out опрашивает очередь раз в 5 с — нарушает правило «без периодического опроса».

## Сделать
- `QueueStore::next_due(kind)` и сигнал `<tenant>/queue/<вид>` при постановке и переносе; testkit, набор, CHANGELOG.

## Приёмка
- CI зелёный; набор: сигнал после постановки, `next_due` — ближайший срок.
