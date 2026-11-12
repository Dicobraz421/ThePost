# CTR-65: входящие планирования: журнал изменений и время получения

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md#Dav]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 55: jmap-19a, PR #493. для `CalendarEventNotification` нет `DavStore::inbox_changes`, `inbox_state` и
`InboxItem.received_at`.

## Сделать
- `DavStore::inbox_state`, `inbox_changes(since)` (добавленные и удалённые с надгробиями), `InboxItem.received_at`;
  по умолчанию — `Internal`. testkit, набор, CHANGELOG. Реализация — META-69.

## Приёмка
- CI зелёный; набор: изменения входящих после состояния.
