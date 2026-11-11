# DAV-09: CardDAV — дом адресных книг и свойства

lane: dav
deps: [DAV-04]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/spec/S4.md#4.8]
checks: [nextest]
manual: no

## Сделать
- `addressbook-home-set`, свойства адресной книги: `supported-address-data` (vCard 3.0 и 4.0), `max-resource-size`,
  `addressbook-description`.
- Адресная книга по умолчанию «Контакты» — при первом обращении к дому.

## Приёмка
- Снимки ответов `PROPFIND` для Thunderbird, Контактов iOS и macOS, DAVx5.
