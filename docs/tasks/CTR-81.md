# CTR-81: действие «поиск» в журнале аудита ящика

lane: contracts
deps: [CTR-35]
class: B
files: [crates/tp-contracts/src/audit.rs, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- `AuditAction::Search` (решение 114); строка запроса — в `AuditObject::detail`, без найденных писем; все `match` по
  `AuditAction` (tp-meta, tp-admin, консоль, выгрузка аудита) — в том же PR.
- В `OWNER` не входит: поиск владельца в своём ящике не журналируется.

## Приёмка
- Общий набор аудита в testkit: запись `Search` сохраняется и читается на обоих хранилищах.
