# JMAP-32: JMAP — общий доступ к календарям и адресным книгам

lane: jmap
deps: [JMAP-31, CTR-48]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.4, docs/spec/S4.md#4.5]
checks: [nextest]
manual: no

## Сделать
- `shareWith` и `myRights` у `Calendar` и `AddressBook` (`CollectionAcl`), `ShareNotification`; общие коллекции — в аккаунтах владельцев
  (`accounts` сессии с `isPersonal` = false).

## Приёмка
- Тесты: выдача и отзыв прав, запись без права — `forbidden`.
