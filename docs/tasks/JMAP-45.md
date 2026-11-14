# JMAP-45: `Principal.type` по типу ресурса — `location` или `resource`

lane: jmap
deps: [CTR-88]
class: A
files: [crates/tp-jmap/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#121, docs/tasks/CTR-88.md, crates/tp-jmap/src/principal.rs]
checks: [nextest]
manual: no

## Сделать
- `Principal.type` (RFC 9670) ресурса — по `Account::resource_type`: помещение — `location`, оборудование — `resource`;
  пользователь — `individual`, как сейчас.
- Фильтр `Principal/query` по `type` — по тем же значениям.

## Приёмка
- Тесты `principal.rs`: `Principal/get` помещения — `location`, оборудования — `resource`; `Principal/query` с
  `{"type": "location"}` и `{"type": "resource"}`.
