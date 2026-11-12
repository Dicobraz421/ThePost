# CTR-49: учётная запись в очереди и учёте BLOB, сигнал ShardsChanged

lane: contracts
deps: [CTR-35]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- `QueueStore::enqueue` и `BlobIndex::acquire` получают учётную запись отправителя/владельца (решение 46);
  `EventKind::ShardsChanged` (решение 47). Реализации в памяти и общий набор тестов; вызовы в крейтах — механически.

## Приёмка
- CI зелёный; `docs/CONTRACTS.md` — раздел и журнал изменений.
