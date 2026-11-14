# CTR-88: тип ресурса бронирования — помещение или оборудование — в контракте

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/meta.rs, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md#121, crates/tp-contracts/src/sharing.rs]
checks: [nextest]
manual: no

## Сделать
- `ResourceType` — `Room` («помещение», по умолчанию) и `Equipment` («оборудование»); serde `room`, `equipment`.
- Поле `Account::resource_type: Option<ResourceType>` и `AccountUpsert::resource_type`: у ресурса
  (`AccountKind::Resource`) — всегда `Some` (нет в запросе — `Room`), у пользователя — `None`; тип у пользователя —
  `Error::Invalid`; смена вида на пользователя снимает тип. Описание — в `docs/CONTRACTS.md` (учётные записи и
  `ResourceStore`).
- Потребители, которые строят `Account` и `AccountUpsert`, правятся в этом же PR (как в CTR-83); реализация в
  `tp-testkit` и тест набора — здесь же. Хранение в PostgreSQL — META-81, Admin API — ADM-47, CalDAV — DAV-38,
  JMAP — JMAP-45.

## Приёмка
- Тест набора: ресурс без типа читается как `Room`; `Equipment` сохраняется и читается; тип у пользователя —
  `Error::Invalid`; смена вида ресурса на пользователя — `resource_type: None`.
