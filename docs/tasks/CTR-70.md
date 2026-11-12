# CTR-70: `TenantTransport.rewrites` без `serde(default)`

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 16; решение 105.

## Сделать
- `#[serde(default)]` у `rewrites` и тест разбора прежнего снимка без поля (`legacy`) удалить.

## Приёмка
- CI зелёный.
