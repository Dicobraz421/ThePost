# DAV-11: CardDAV — версии vCard и проверка карточек

lane: dav
deps: [DAV-10]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/spec/S4.md#4.8]
checks: [nextest]
manual: docs/manual-tests/DAV-11.md

## Сделать
- Выдача в версии по `Accept`/`address-data` (`text/vcard; version=3.0|4.0`) — преобразование calcard; хранится принятая версия.
- Предусловия `PUT` (RFC 6352 §6.3.2): `valid-address-data`, `no-uid-conflict`, `max-resource-size`; размер встроенного фото —
  не больше `dav.max_photo_size`.
- Ручная проверка контактов на стенде (после APP-25): Thunderbird, Контакты iOS и macOS, DAVx5, Evolution — создание, фото, группы
  (Apple `X-ADDRESSBOOKSERVER-KIND`), удаление, синхронизация двух устройств.

## Приёмка
- Тесты преобразования 3.0 ↔ 4.0 без потери полей корпуса; предусловия; сценарии ручной проверки без замечаний.
