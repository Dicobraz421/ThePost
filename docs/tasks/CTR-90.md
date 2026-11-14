# CTR-90: вытеснение старых `ShareNotification` сверх предела в хранилище

lane: contracts
deps: [CTR-86, META-83]
class: B
files: [crates/tp-contracts/src/sharing.rs, crates/tp-testkit/src/sharing.rs, crates/tp-testkit/src/suite/sharing.rs, crates/tp-meta/src/*, docs/CONTRACTS.md]
read: [docs/tasks/CTR-86.md, docs/tasks/JMAP-46.md]
checks: [nextest]
manual: no

## Сделать
- В CTR-86 вошёл только срок хранения. Добавить предел числа уведомлений на учётную запись (`MAX_NOTICES` = 1000,
  константа в контракте): сверх предела `put` вытесняет старшие, вытесненное попадает в `destroyed` как при удалении
  (`changes` не теряет событие). Старая константа в `tp-jmap` удалена вместе с JMAP-46.
- Реализации правятся в этом же PR (класс B): память `tp-testkit` и `PgMeta`; описание в `docs/CONTRACTS.md`.

## Приёмка
- Тест набора `suite::sharing::share_notifications`: запись 1001-го уведомления вытесняет самое старое, `changes`
  отдаёт его в `destroyed`; проходит на памяти и на PostgreSQL.
