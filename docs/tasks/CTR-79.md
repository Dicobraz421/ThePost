# CTR-79: политика: лимит «получателей в час»

lane: contracts
deps: [CTR-25]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Поле `Policy::rcpts_per_hour` (организация → домен → ящик, по умолчанию 2 000) — решение 113; все потребители
  `Policy` (tp-meta, tp-admin, tp-smtp, testkit) — в том же PR (правило 12.11).

## Приёмка
- Общий набор политик в testkit проверяет наследование нового поля.
