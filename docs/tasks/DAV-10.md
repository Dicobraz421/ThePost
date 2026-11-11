# DAV-10: CardDAV — addressbook-query и addressbook-multiget

lane: dav
deps: [DAV-09]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

## Сделать
- `REPORT addressbook-query` (RFC 6352 §8.6): `prop-filter`, `param-filter`, `text-match` (`equals`, `contains`, `starts-with`,
  `ends-with`), `test="anyof|allof"`, `limit`; `address-data` с выбором свойств.
- `REPORT addressbook-multiget` (RFC 6352 §8.7).

## Приёмка
- Тесты фильтров на наборе карточек (кириллица, несколько адресов и телефонов).
