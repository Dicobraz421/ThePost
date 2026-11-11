# JMAP-25: JMAP Contacts — AddressBook (RFC 9610)

lane: jmap
deps: [CTR-45, JMAP-02]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Capability `urn:ietf:params:jmap:contacts`: `AddressBook/get`, `/changes`, `/set` (имя, описание, `isDefault`, `onDestroyRemoveContents`)
  поверх `DavStore` (вид `AddressBook`); `myRights` — свои; общий доступ — JMAP-32.
- Типы push `AddressBook`, `ContactCard` по `DavChanged`.

## Приёмка
- Тесты на in-memory: создание, переименование, удаление с содержимым и без, `/changes`.
