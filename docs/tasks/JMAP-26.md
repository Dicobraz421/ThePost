# JMAP-26: JMAP Contacts — ContactCard

lane: jmap
deps: [JMAP-25, DAV-03]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `ContactCard/get`, `/changes`, `/query` (фильтры RFC 9610 §3.4: `inAddressBook`, `uid`, `text`, `name`, `email`, `phone`; сортировка),
  `/queryChanges`, `/set`, `/copy`; JSContact ↔ vCard — calcard; хранится vCard (общий объект с CardDAV), индекс — `tp_dav::index`.
- Глобальная адресная книга — только чтение (`mayWrite` = false).

## Приёмка
- Тесты: карточка из JMAP читается CardDAV и наоборот без потери полей корпуса; запрос по имени и адресу.
