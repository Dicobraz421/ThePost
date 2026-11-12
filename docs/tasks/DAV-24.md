# DAV-24: CardDAV — поисковые строки контакта и `addressbook-query` по индексу

lane: dav
deps: [CTR-54, DAV-10]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md#Dav]
checks: [nextest]
manual: no

Хвост волны jmap-16, PR #428.

## Сделать
- `index.rs::card`: заполняет поисковые строки `ObjectIndex` (CTR-54) — полное имя, части имени, адреса e-mail.
- `addressbook-query` с `text-match` по `FN`, `N`, `EMAIL`: кандидаты — `DavStore::find_cards`, точная проверка
  фильтра — как сейчас; остальные фильтры — прежним перебором.

## Приёмка
- Тесты: `PUT` vCard даёт индекс с именем и адресами; `addressbook-query` по части имени и по адресу возвращает те
  же объекты, что полный перебор.
